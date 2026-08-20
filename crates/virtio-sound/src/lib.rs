//! virtio-snd device (backlog GAME-2102): the guest can play audio the host
//! hears.
//!
//! Four layers, the same shape every device crate in this workspace has:
//!
//! * [`protocol`] — the wire format (VirtIO spec 1.2, section 5.14): constants,
//!   fixed-size structs, lengths asserted at compile time. Pure and portable.
//! * [`stream`] — PCM parameters, the bounds a hostile guest is held to, and
//!   the stream lifecycle as a pure state machine.
//! * [`backend`] — [`AudioSink`], the host playback contract, plus the two
//!   software sinks ([`NullSink`], [`RecordingSink`]) that make the whole
//!   device testable on a machine with no sound card at all. The real sinks
//!   are [`alsa::AlsaSink`] (Linux) and [`wasapi::WasapiSink`] (Windows).
//! * [`device`] — [`SoundDevice`], the `virtio_core::VirtioDevice`
//!   implementation, and the pump thread that turns "bytes copied" into
//!   "audio played".
//!
//! Nothing here knows about virtio-mmio or virtio-pci: the device sees queues,
//! features and its config space, so both transports drive it unchanged.
//!
//! # What this phase implements, and what it does not
//!
//! **Implemented:** all four queues; jack, PCM and channel-map information
//! messages; the full `SET_PARAMS` / `PREPARE` / `START` / `STOP` / `RELEASE`
//! lifecycle; and period-based playback on the TX queue, where a message is
//! returned to the guest only once its audio has actually been consumed by the
//! host.
//!
//! **Deferred to phase 2, deliberately and completely rather than half-done:**
//!
//! * **Capture (RX).** The config space advertises one *output* stream and no
//!   input stream, and the channel map is an output map, so a conforming
//!   driver never posts a capture buffer. One that does is answered
//!   `VIRTIO_SND_S_NOT_SUPP` in band. Adding capture means a second stream in
//!   the info table, an input chmap, an `AudioSource` half of the sink trait
//!   and a pump that fills guest buffers — none of which is stubbed here.
//! * **Controls** (`VIRTIO_SND_F_CTLS`): no mixer elements, so the guest's
//!   volume slider is its own software mixer. Not offered as a feature.
//! * **Shared-memory and event-based transfer** (`VIRTIO_SND_PCM_F_SHMEM_*`,
//!   `F_EVT_*`, `F_MSG_POLLING`): not offered, so the driver stays on the
//!   plain message path.
//! * **Formats beyond `S16`** and **rates beyond 44100/48000**: the guest's
//!   own ALSA converts, and every extra format is more host code on an
//!   untrusted path. See [`stream::SUPPORTED_FORMATS`].
//!
//! # The Linux licence decision
//!
//! `libasound` is LGPL and `cargo deny check` blocks copyleft in the host's
//! dependency graph, so the ALSA sink `dlopen`s the host's library at runtime
//! and links nothing — the arrangement ADR-0004 already settled on for
//! virglrenderer. The full reasoning, including why PipeWire's MIT client
//! library was *not* the answer, is in [`alsa`]'s module docs.

pub mod backend;
pub mod device;
pub mod protocol;
pub mod stream;

#[cfg(target_os = "linux")]
pub mod alsa;
#[cfg(windows)]
pub mod wasapi;

pub use backend::{
    AudioError, AudioSink, NullSink, Pacer, Recording, RecordingSink, StreamFormat,
    MAX_RECORDING_BYTES,
};
pub use device::{
    SinkFactory, SoundDevice, SoundStats, CHAINS_PER_NOTIFY, MAX_CONTROL_MSG_BYTES,
    MAX_PENDING_PERIODS, MAX_XFER_BYTES, NUM_QUEUES,
};
pub use stream::{
    ParamError, PcmParams, StreamState, CHMAPS, JACKS, MAX_BUFFER_BYTES, MAX_CHANNELS, MAX_PERIODS,
    MAX_PERIOD_BYTES, MIN_PERIODS, MIN_PERIOD_BYTES, STREAMS, SUPPORTED_FORMATS, SUPPORTED_RATES,
};

/// Which host sink a VM should use.
///
/// Kept here rather than in `control-api` so the *resolution* — which backend
/// exists on this host, and what to do when the chosen one does not — lives
/// next to the sinks it names. `control-api` only has to parse a word.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum SinkChoice {
    /// The host's native backend if it is there, silence (with a warning) if
    /// it is not. Audio is not a boot dependency: a VM must still run on a
    /// machine with no sound card, over SSH, or in CI.
    #[default]
    Auto,
    /// Silence, but paced like a sound card. What a headless run wants.
    Null,
    /// ALSA (Linux only). Fails loudly if libasound is missing, because an
    /// explicit choice that silently did something else is the bug the option
    /// exists to avoid.
    Alsa,
    /// WASAPI (Windows only), same policy.
    Wasapi,
}

impl std::fmt::Display for SinkChoice {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(match self {
            Self::Auto => "auto",
            Self::Null => "null",
            Self::Alsa => "alsa",
            Self::Wasapi => "wasapi",
        })
    }
}

/// Resolves a [`SinkChoice`] into a name and a [`SinkFactory`].
///
/// The factory is called on the pump thread, once per activation. This
/// function is where the *first* attempt happens, so a host problem is a
/// startup error rather than a silent stream of nothing — except under
/// [`SinkChoice::Auto`], which is allowed to fall back.
pub fn open_sink(choice: SinkChoice) -> Result<(String, SinkFactory), AudioError> {
    match choice {
        SinkChoice::Null => Ok(null_factory()),
        SinkChoice::Alsa => alsa_factory(),
        SinkChoice::Wasapi => wasapi_factory(),
        SinkChoice::Auto => match native_factory() {
            None => Ok(null_factory()),
            Some(Ok(resolved)) => Ok(resolved),
            Some(Err(error)) => {
                tracing::warn!(
                    %error,
                    "no host audio backend; the guest gets a sound card that plays into silence"
                );
                Ok(null_factory())
            }
        },
    }
}

fn null_factory() -> (String, SinkFactory) {
    (
        "null".to_owned(),
        std::sync::Arc::new(|| Box::new(NullSink::new()) as Box<dyn AudioSink>),
    )
}

/// The native backend of *this* host, or `None` if it has none.
fn native_factory() -> Option<Result<(String, SinkFactory), AudioError>> {
    #[cfg(target_os = "linux")]
    return Some(alsa_factory());
    #[cfg(windows)]
    return Some(wasapi_factory());
    #[cfg(not(any(target_os = "linux", windows)))]
    None
}

#[cfg(target_os = "linux")]
fn alsa_factory() -> Result<(String, SinkFactory), AudioError> {
    // Resolved once here so a missing libasound is a startup error; the pump
    // resolves it again on its own thread, which is where a sink must live.
    let name = alsa::AlsaSink::load()?.name().to_owned();
    let factory: SinkFactory = std::sync::Arc::new(|| match alsa::AlsaSink::load() {
        Ok(sink) => Box::new(sink) as Box<dyn AudioSink>,
        Err(error) => {
            tracing::warn!(%error, "ALSA disappeared between startup and use");
            Box::new(NullSink::new()) as Box<dyn AudioSink>
        }
    });
    Ok((name, factory))
}

#[cfg(not(target_os = "linux"))]
fn alsa_factory() -> Result<(String, SinkFactory), AudioError> {
    Err(AudioError::Unavailable(
        "[sound] backend = \"alsa\" is Linux-only; use \"auto\" or \"null\"".into(),
    ))
}

#[cfg(windows)]
fn wasapi_factory() -> Result<(String, SinkFactory), AudioError> {
    let name = wasapi::WasapiSink::probe()?;
    let factory: SinkFactory =
        std::sync::Arc::new(|| Box::new(wasapi::WasapiSink::new()) as Box<dyn AudioSink>);
    Ok((name, factory))
}

#[cfg(not(windows))]
fn wasapi_factory() -> Result<(String, SinkFactory), AudioError> {
    Err(AudioError::Unavailable(
        "[sound] backend = \"wasapi\" is Windows-only; use \"auto\" or \"null\"".into(),
    ))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_null_choice_always_resolves() {
        let (name, factory) = open_sink(SinkChoice::Null).expect("null always works");
        assert_eq!(name, "null");
        assert_eq!(factory().name(), "null");
    }

    /// `auto` must never fail: a VM has to boot on a machine with no speakers.
    #[test]
    fn auto_always_resolves_to_something() {
        let (name, factory) = open_sink(SinkChoice::Auto).expect("auto never fails");
        assert!(!name.is_empty());
        let sink = factory();
        assert!(!sink.name().is_empty());
    }

    /// And an explicit choice for the *other* host's backend is refused rather
    /// than quietly turned into silence.
    #[test]
    fn an_explicit_backend_from_the_wrong_host_is_refused() {
        #[cfg(target_os = "linux")]
        assert!(open_sink(SinkChoice::Wasapi).is_err());
        #[cfg(windows)]
        assert!(open_sink(SinkChoice::Alsa).is_err());
    }

    #[test]
    fn choices_print_as_the_words_the_config_uses() {
        assert_eq!(SinkChoice::Auto.to_string(), "auto");
        assert_eq!(SinkChoice::Null.to_string(), "null");
        assert_eq!(SinkChoice::Alsa.to_string(), "alsa");
        assert_eq!(SinkChoice::Wasapi.to_string(), "wasapi");
    }
}
