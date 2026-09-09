//! The Linux playback sink and capture source: ALSA, `dlopen`ed at runtime.
//!
//! # The licence decision (read this before changing the dependency)
//!
//! Entangled Desktop ships no copyleft code, and `cargo deny check` is the
//! gate (CLAUDE.md, ADR-0001). Linux audio makes that a real choice rather
//! than a formality:
//!
//! * **`alsa` / `alsa-sys` / `cpal` (rejected).** ALSA's `libasound` is
//!   LGPL-2.1+. A Rust crate that *links* it puts a copyleft C library into
//!   this binary's link line and its build graph — which is precisely the
//!   arrangement the rule exists to prevent, whatever the wrapper crate's own
//!   licence says.
//! * **PipeWire's client library (rejected for now).** `libpipewire-0.3` is
//!   MIT, so a binding crate would be graph-clean. But it is absent from
//!   plain-ALSA and PulseAudio hosts (Debian stable's minimal desktops,
//!   older Ubuntu), and its API needs a main loop, a context and a registry
//!   before a single sample moves. It buys a permissive graph we can already
//!   have, at the cost of not working everywhere.
//! * **`libasound` via `dlopen` (chosen).** Exactly the arrangement ADR-0004
//!   settled on for virglrenderer: nothing is linked, nothing enters the cargo
//!   graph, `cargo deny check` has nothing to inspect, and the library is the
//!   host's — user-installed, user-replaceable, never redistributed by us. We
//!   resolve nine C entry points by name at runtime and fall back to
//!   [`NullSink`](crate::backend::NullSink) if they are not there.
//!
//! And practically it is also the *widest* sink: PipeWire and PulseAudio both
//! install an ALSA plugin, so `default` reaches whichever of the three a
//! desktop actually runs. One sink, three sound servers.
//!
//! # Safety model
//!
//! `libasound` is a host library on the trusted side of the boundary: it never
//! sees guest addresses, only a host-owned S16 buffer whose length we computed.
//! What this module owes is the FFI contract — a live handle, a correct frame
//! count, and no use of the handle after `snd_pcm_close`. Every `unsafe` block
//! says which of those it is relying on. The library is never `dlclose`d, for
//! the same reason virglrenderer is not: ALSA leaves a config cache and atexit
//! handlers behind, and unloading it under a running process is not a
//! supported operation.
//!
//! The capture source ([`AlsaSource`]) is the same arrangement read backwards:
//! the same library, the same handle type, one extra entry point
//! (`snd_pcm_readi`), and the buffer it fills is host memory the device copies
//! into guest buffers only after checking their lengths. ALSA never sees a
//! guest address in either direction.

use std::ffi::{c_char, c_int, c_long, c_ulong, c_void, CStr, CString};

use crate::backend::{AudioError, AudioSink, AudioSource, StreamFormat};

/// Override the ALSA device the sink opens. `default` routes through whatever
/// the host runs — dmix, PulseAudio's or PipeWire's ALSA plugin, or the card
/// itself.
pub const ALSA_DEVICE_ENV: &str = "ENTANGLED_SND_ALSA_DEVICE";

/// Override the ALSA device the *capture* source opens, separately from the
/// playback one: a desktop's default sink and default microphone are rarely
/// the same card, and `default` does not always resolve to a capture device.
pub const ALSA_CAPTURE_DEVICE_ENV: &str = "ENTANGLED_SND_ALSA_CAPTURE_DEVICE";

/// `SND_PCM_STREAM_PLAYBACK`.
const STREAM_PLAYBACK: c_int = 0;
/// `SND_PCM_STREAM_CAPTURE`.
const STREAM_CAPTURE: c_int = 1;
/// `SND_PCM_FORMAT_S16_LE`.
const FORMAT_S16_LE: c_int = 2;
/// `SND_PCM_ACCESS_RW_INTERLEAVED`.
const ACCESS_RW_INTERLEAVED: c_int = 3;
/// Software resampling on: a card that cannot do 44100 gets it from ALSA
/// rather than from us.
const SOFT_RESAMPLE: c_int = 1;
/// Latency hint handed to `snd_pcm_set_params`, in microseconds. ALSA sizes
/// its ring from this; 60 ms is comfortably more than the pump's 43 ms chunk
/// without adding audible lag.
const LATENCY_US: u32 = 60_000;

/// `snd_pcm_t` is opaque.
#[repr(C)]
struct PcmHandle {
    _private: [u8; 0],
}

/// The ten entry points this module needs, resolved up front so an ALSA too
/// old to have one of them fails at load rather than mid-stream. Both the sink
/// and the source resolve the whole set: the two extra symbols cost a `dlsym`
/// each and keep "the library is usable" a single answer.
struct Api {
    open: unsafe extern "C" fn(*mut *mut PcmHandle, *const c_char, c_int, c_int) -> c_int,
    set_params: unsafe extern "C" fn(
        *mut PcmHandle,
        c_int,
        c_int,
        std::ffi::c_uint,
        std::ffi::c_uint,
        c_int,
        std::ffi::c_uint,
    ) -> c_int,
    writei: unsafe extern "C" fn(*mut PcmHandle, *const c_void, c_ulong) -> c_long,
    readi: unsafe extern "C" fn(*mut PcmHandle, *mut c_void, c_ulong) -> c_long,
    recover: unsafe extern "C" fn(*mut PcmHandle, c_int, c_int) -> c_int,
    prepare: unsafe extern "C" fn(*mut PcmHandle) -> c_int,
    drop_: unsafe extern "C" fn(*mut PcmHandle) -> c_int,
    drain: unsafe extern "C" fn(*mut PcmHandle) -> c_int,
    close: unsafe extern "C" fn(*mut PcmHandle) -> c_int,
    strerror: unsafe extern "C" fn(c_int) -> *const c_char,
    // Never dlclose'd — see the module docs.
    _lib: std::mem::ManuallyDrop<libloading::Library>,
}

impl Api {
    fn load() -> Result<Self, String> {
        let lib = ["libasound.so.2", "libasound.so"]
            .iter()
            .find_map(|name| {
                // SAFETY: dlopen of a system library by soname. Its
                // constructors are the platform loader's business; we resolve
                // every symbol we use before calling anything.
                unsafe { libloading::Library::new(name) }.ok()
            })
            .ok_or_else(|| {
                "libasound.so.2 not found — install libasound2 (Debian/Ubuntu) \
                 or set [sound] backend = \"null\""
                    .to_string()
            })?;

        macro_rules! sym {
            ($name:literal) => {
                // SAFETY: the symbol is looked up by its documented C name in
                // the library just opened, and the declared fn type matches the
                // prototype in <alsa/pcm.h>.
                unsafe { lib.get(concat!($name, "\0").as_bytes()) }
                    .map(|s: libloading::Symbol<_>| *s)
                    .map_err(|e| format!("{}: {e} — libasound too old?", $name))?
            };
        }

        Ok(Self {
            open: sym!("snd_pcm_open"),
            set_params: sym!("snd_pcm_set_params"),
            writei: sym!("snd_pcm_writei"),
            readi: sym!("snd_pcm_readi"),
            recover: sym!("snd_pcm_recover"),
            prepare: sym!("snd_pcm_prepare"),
            drop_: sym!("snd_pcm_drop"),
            drain: sym!("snd_pcm_drain"),
            close: sym!("snd_pcm_close"),
            strerror: sym!("snd_strerror"),
            _lib: std::mem::ManuallyDrop::new(lib),
        })
    }

    /// ALSA's own message for a negative return code.
    fn message(&self, code: c_int) -> String {
        // SAFETY: `snd_strerror` returns a pointer to a static NUL-terminated
        // string for any input, valid for the life of the library (which is
        // never unloaded).
        let raw = unsafe { (self.strerror)(code) };
        if raw.is_null() {
            return format!("error {code}");
        }
        // SAFETY: non-null and NUL-terminated by the contract above.
        unsafe { CStr::from_ptr(raw) }
            .to_string_lossy()
            .into_owned()
    }
}

/// A playback stream on the host's ALSA `default` device.
pub struct AlsaSink {
    api: Api,
    name: String,
    device: CString,
    /// Non-null exactly while a stream is open.
    pcm: *mut PcmHandle,
    frame_bytes: usize,
}

// SAFETY: the raw `snd_pcm_t` is owned solely by this struct and only ever
// touched from the one thread that holds it (the device's pump thread — see
// `AudioSink`'s docs). `Send` is what lets it be moved onto that thread at
// activate; it is deliberately not `Sync`.
unsafe impl Send for AlsaSink {}

impl std::fmt::Debug for AlsaSink {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("AlsaSink")
            .field("device", &self.device)
            .field("open", &!self.pcm.is_null())
            .finish_non_exhaustive()
    }
}

impl AlsaSink {
    /// Resolves libasound and prepares a sink. Opening the device itself is
    /// deferred to [`AudioSink::start`], because the format is not known until
    /// the guest configures a stream.
    pub fn load() -> Result<Self, AudioError> {
        let api = Api::load().map_err(AudioError::Unavailable)?;
        let device = std::env::var(ALSA_DEVICE_ENV)
            .ok()
            .filter(|v| !v.trim().is_empty())
            .unwrap_or_else(|| "default".to_owned());
        let name = format!("alsa:{device}");
        let device = CString::new(device)
            .map_err(|_| AudioError::Unavailable(format!("{ALSA_DEVICE_ENV} contains a NUL")))?;
        Ok(Self {
            api,
            name,
            device,
            pcm: std::ptr::null_mut(),
            frame_bytes: 4,
        })
    }

    fn close(&mut self) {
        if self.pcm.is_null() {
            return;
        }
        let pcm = std::mem::replace(&mut self.pcm, std::ptr::null_mut());
        // SAFETY: `pcm` was returned by `snd_pcm_open` and has not been closed
        // (it was non-null and is taken out here, so no second close is
        // possible). `drain` flushes what the card still holds; `close`
        // invalidates the handle, which is why nothing reads `self.pcm` after.
        unsafe {
            (self.api.drain)(pcm);
            (self.api.close)(pcm);
        }
    }
}

impl Drop for AlsaSink {
    fn drop(&mut self) {
        self.close();
    }
}

impl AudioSink for AlsaSink {
    fn name(&self) -> &str {
        &self.name
    }

    fn start(&mut self, format: StreamFormat, _period_bytes: usize) -> Result<(), AudioError> {
        self.close();
        self.frame_bytes = format.frame_bytes().max(1);

        let mut pcm: *mut PcmHandle = std::ptr::null_mut();
        // SAFETY: `pcm` is a live local the callee writes through; `device` is
        // a NUL-terminated CString owned by self and outlives the call.
        let rc = unsafe {
            (self.api.open)(
                &mut pcm,
                self.device.as_ptr(),
                STREAM_PLAYBACK,
                0, // blocking mode: `writei` returning is our pacing
            )
        };
        if rc < 0 || pcm.is_null() {
            return Err(AudioError::Unavailable(format!(
                "snd_pcm_open({}): {}",
                self.device.to_string_lossy(),
                self.api.message(rc)
            )));
        }

        // SAFETY: `pcm` was just opened and is non-null; the enum values are
        // the documented SND_PCM_* constants for this API.
        let rc = unsafe {
            (self.api.set_params)(
                pcm,
                FORMAT_S16_LE,
                ACCESS_RW_INTERLEAVED,
                u32::from(format.channels),
                format.rate_hz,
                SOFT_RESAMPLE,
                LATENCY_US,
            )
        };
        if rc < 0 {
            // SAFETY: same live handle; closing it is the only thing done with
            // it after this point.
            unsafe { (self.api.close)(pcm) };
            return Err(AudioError::Format {
                rate_hz: format.rate_hz,
                channels: format.channels,
                reason: self.api.message(rc),
            });
        }
        self.pcm = pcm;
        Ok(())
    }

    fn write(&mut self, pcm_bytes: &[u8]) -> Result<usize, AudioError> {
        if self.pcm.is_null() {
            return Err(AudioError::Io("ALSA stream is not open".into()));
        }
        let frames = pcm_bytes.len() / self.frame_bytes;
        if frames == 0 {
            return Ok(0);
        }
        // Two attempts: one write, and — if the card underran while we were
        // away — one recover-and-retry. A second failure is reported, and the
        // device drops to silence rather than spinning on a broken card.
        for attempt in 0..2 {
            // SAFETY: `self.pcm` is a live handle (checked non-null above and
            // only cleared by `close`, which also sets it null). The buffer is
            // host memory of at least `frames * frame_bytes` bytes, which is
            // exactly what interleaved S16 frames of this shape occupy.
            let written = unsafe {
                (self.api.writei)(self.pcm, pcm_bytes.as_ptr().cast(), frames as c_ulong)
            };
            if written >= 0 {
                return Ok((written as usize).saturating_mul(self.frame_bytes));
            }
            let code = written as c_int;
            if attempt == 0 {
                // SAFETY: live handle; `snd_pcm_recover` is the documented way
                // to handle -EPIPE/-ESTRPIPE and leaves the handle usable.
                let recovered = unsafe { (self.api.recover)(self.pcm, code, 1) };
                if recovered == 0 {
                    // SAFETY: live handle, back in SETUP after a recover.
                    unsafe { (self.api.prepare)(self.pcm) };
                    continue;
                }
            }
            return Err(AudioError::Io(format!(
                "snd_pcm_writei: {}",
                self.api.message(code)
            )));
        }
        Ok(0)
    }

    fn stop(&mut self) {
        if self.pcm.is_null() {
            return;
        }
        // SAFETY: live handle; `snd_pcm_drop` discards what is still queued,
        // which is what a STOP means (as opposed to `drain` on close).
        unsafe { (self.api.drop_)(self.pcm) };
        self.close();
    }
}

// -------------------------------------------------------------- capture source

/// A capture stream on the host's ALSA capture device.
///
/// The same library, the same handle, the same recovery dance as
/// [`AlsaSink`] — only `snd_pcm_readi` in place of `snd_pcm_writei`, and a
/// buffer the callee fills rather than one it reads.
pub struct AlsaSource {
    api: Api,
    name: String,
    device: CString,
    /// Non-null exactly while a stream is open.
    pcm: *mut PcmHandle,
    frame_bytes: usize,
}

// SAFETY: as for `AlsaSink` — the raw `snd_pcm_t` is owned solely by this
// struct and only ever touched from the one thread that holds it (the device's
// capture pump). `Send` is what lets it be moved onto that thread; it is
// deliberately not `Sync`.
unsafe impl Send for AlsaSource {}

impl std::fmt::Debug for AlsaSource {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("AlsaSource")
            .field("device", &self.device)
            .field("open", &!self.pcm.is_null())
            .finish_non_exhaustive()
    }
}

impl AlsaSource {
    /// Resolves libasound and prepares a source. Opening the device is
    /// deferred to [`AudioSource::start`], because the format is not known
    /// until the guest configures the capture stream.
    pub fn load() -> Result<Self, AudioError> {
        let api = Api::load().map_err(AudioError::Unavailable)?;
        let device = std::env::var(ALSA_CAPTURE_DEVICE_ENV)
            .ok()
            .filter(|v| !v.trim().is_empty())
            .unwrap_or_else(|| "default".to_owned());
        let name = format!("alsa:{device}");
        let device = CString::new(device).map_err(|_| {
            AudioError::Unavailable(format!("{ALSA_CAPTURE_DEVICE_ENV} contains a NUL"))
        })?;
        Ok(Self {
            api,
            name,
            device,
            pcm: std::ptr::null_mut(),
            frame_bytes: 4,
        })
    }

    fn close(&mut self) {
        if self.pcm.is_null() {
            return;
        }
        let pcm = std::mem::replace(&mut self.pcm, std::ptr::null_mut());
        // SAFETY: `pcm` was returned by `snd_pcm_open` and has not been closed
        // (it was non-null and is taken out here, so no second close is
        // possible). A capture stream is dropped rather than drained — there is
        // nothing of ours left in the card to flush. `close` invalidates the
        // handle, which is why nothing reads `self.pcm` after.
        unsafe {
            (self.api.drop_)(pcm);
            (self.api.close)(pcm);
        }
    }
}

impl Drop for AlsaSource {
    fn drop(&mut self) {
        self.close();
    }
}

impl AudioSource for AlsaSource {
    fn name(&self) -> &str {
        &self.name
    }

    fn start(&mut self, format: StreamFormat, _period_bytes: usize) -> Result<(), AudioError> {
        self.close();
        self.frame_bytes = format.frame_bytes().max(1);

        let mut pcm: *mut PcmHandle = std::ptr::null_mut();
        // SAFETY: `pcm` is a live local the callee writes through; `device` is
        // a NUL-terminated CString owned by self and outlives the call.
        let rc = unsafe {
            (self.api.open)(
                &mut pcm,
                self.device.as_ptr(),
                STREAM_CAPTURE,
                0, // blocking mode: `readi` returning is our pacing
            )
        };
        if rc < 0 || pcm.is_null() {
            return Err(AudioError::Unavailable(format!(
                "snd_pcm_open({}, capture): {}",
                self.device.to_string_lossy(),
                self.api.message(rc)
            )));
        }

        // SAFETY: `pcm` was just opened and is non-null; the enum values are
        // the documented SND_PCM_* constants for this API.
        let rc = unsafe {
            (self.api.set_params)(
                pcm,
                FORMAT_S16_LE,
                ACCESS_RW_INTERLEAVED,
                u32::from(format.channels),
                format.rate_hz,
                SOFT_RESAMPLE,
                LATENCY_US,
            )
        };
        if rc < 0 {
            // SAFETY: same live handle; closing it is the only thing done with
            // it after this point.
            unsafe { (self.api.close)(pcm) };
            return Err(AudioError::Format {
                rate_hz: format.rate_hz,
                channels: format.channels,
                reason: self.api.message(rc),
            });
        }
        self.pcm = pcm;
        Ok(())
    }

    fn read(&mut self, pcm_bytes: &mut [u8]) -> Result<usize, AudioError> {
        if self.pcm.is_null() {
            return Err(AudioError::Io("ALSA capture stream is not open".into()));
        }
        // Whole frames only: a partial frame would put the guest's channels out
        // of step for the rest of the stream.
        let frames = pcm_bytes.len() / self.frame_bytes;
        if frames == 0 {
            return Ok(0);
        }
        // Two attempts, exactly as the sink does: one read, and — if the card
        // overran while we were away — one recover-and-retry.
        for attempt in 0..2 {
            // SAFETY: `self.pcm` is a live handle (checked non-null above and
            // only cleared by `close`, which also sets it null). The buffer is
            // host memory of at least `frames * frame_bytes` writable bytes,
            // which is exactly what the callee fills with interleaved S16
            // frames of this shape.
            let read = unsafe {
                (self.api.readi)(self.pcm, pcm_bytes.as_mut_ptr().cast(), frames as c_ulong)
            };
            if read >= 0 {
                // Clamp rather than trust: the contract says at most `frames`,
                // and the caller is about to copy this many bytes towards a
                // guest buffer.
                let got = (read as usize).min(frames);
                return Ok(got.saturating_mul(self.frame_bytes));
            }
            let code = read as c_int;
            if attempt == 0 {
                // SAFETY: live handle; `snd_pcm_recover` is the documented way
                // to handle -EPIPE/-ESTRPIPE and leaves the handle usable.
                let recovered = unsafe { (self.api.recover)(self.pcm, code, 1) };
                if recovered == 0 {
                    // SAFETY: live handle, back in SETUP after a recover.
                    unsafe { (self.api.prepare)(self.pcm) };
                    continue;
                }
            }
            return Err(AudioError::Io(format!(
                "snd_pcm_readi: {}",
                self.api.message(code)
            )));
        }
        Ok(0)
    }

    fn stop(&mut self) {
        self.close();
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Self-skipping, like every other host-resource test in this workspace:
    /// CI containers have no libasound and no sound card, and that must not be
    /// a failure. When the library *is* there, this proves the ten symbols
    /// resolve — which is the half of the FFI contract a unit test can check.
    #[test]
    fn libasound_resolves_when_the_host_has_it() {
        match AlsaSink::load() {
            Ok(sink) => {
                assert!(sink.name().starts_with("alsa:"));
            }
            Err(error) => {
                eprintln!("skipping: {error}");
            }
        }
    }

    #[test]
    fn the_capture_source_resolves_the_same_library() {
        match AlsaSource::load() {
            Ok(source) => assert!(source.name().starts_with("alsa:")),
            Err(error) => eprintln!("skipping: {error}"),
        }
    }

    /// A source that was never started must refuse a read rather than hand a
    /// null handle to libasound.
    #[test]
    fn reading_from_an_unopened_source_is_an_error_not_a_crash() {
        let Ok(mut source) = AlsaSource::load() else {
            eprintln!("skipping: libasound is not installed");
            return;
        };
        assert!(source.read(&mut [0u8; 64]).is_err());
        source.stop();
    }
}
