//! PCM stream parameters, bounds and the lifecycle state machine.
//!
//! Everything here is pure: no guest memory, no host audio, no queues. That
//! makes it the natural place for the "guest is hostile" rules, and it is what
//! the `snd_control` fuzz target drives directly.
//!
//! # What the device advertises, and why so little
//!
//! One output stream and one input stream, `S16` samples, 44100 or 48000 Hz,
//! one or two channels — the *same* set in both directions, deliberately. A
//! guest ALSA stack converts anything else in userspace before it ever
//! reaches the device, so a longer list buys nothing but more host code on an
//! untrusted path — the same "correct first, fast later" call `virtio-net`
//! made about offloads. The advertised set lives in [`SUPPORTED_FORMATS`] /
//! [`SUPPORTED_RATES`] and every request is checked against *those constants*
//! rather than against whatever the guest claims we said.

use crate::protocol::{
    self, rate_hz, D_INPUT, D_OUTPUT, FMT_S16, PCM_F_EVT_SHMEM_PERIODS, PCM_F_EVT_XRUNS,
    PCM_F_MSG_POLLING, PCM_F_SHMEM_GUEST, PCM_F_SHMEM_HOST, RATE_44100, RATE_48000, S_BAD_MSG,
    S_NOT_SUPP,
};

/// Streams this device advertises: two — one playback, one capture.
///
/// The identifiers are fixed, not discovered: [`OUTPUT_STREAM`] then
/// [`INPUT_STREAM`], and `PCM_INFO` reports them in that order. Every
/// identifier a guest sends is checked against this count, and every I/O
/// message additionally against the direction of the queue it arrived on.
pub const STREAMS: u32 = 2;
/// The playback stream's identifier.
pub const OUTPUT_STREAM: u32 = 0;
/// The capture stream's identifier.
pub const INPUT_STREAM: u32 = 1;
/// Physical jacks advertised: a line-out and a microphone, in that order.
pub const JACKS: u32 = 2;
/// Channel maps advertised: one per stream, in stream order.
pub const CHMAPS: u32 = 2;

/// Which way a stream identifier points, or `None` when it names no stream we
/// advertise.
///
/// The device's whole notion of "this message arrived on the wrong queue"
/// comes from here, so it is a function of the *constant* identifiers and
/// never of anything the guest asserts.
pub const fn direction_of(stream_id: u32) -> Option<u8> {
    match stream_id {
        OUTPUT_STREAM => Some(D_OUTPUT),
        INPUT_STREAM => Some(D_INPUT),
        _ => None,
    }
}

/// Channels one stream may carry. Two, because that is what the chmap
/// describes and what every host sink accepts without a matrix mixer.
pub const MAX_CHANNELS: u8 = 2;
/// Smallest channel count.
pub const MIN_CHANNELS: u8 = 1;

/// Bytes per sample of the one format we advertise.
pub const BYTES_PER_SAMPLE: u32 = 2;

/// Smallest period the device will accept. Below this the pump thread would
/// wake more often than it can usefully do work; 64 bytes is 16 stereo frames.
pub const MIN_PERIOD_BYTES: u32 = 64;
/// Largest period the device will accept — and therefore the largest payload
/// one playback message may carry, which is what bounds the per-message
/// staging buffer. 64 KiB is ~340 ms of 48 kHz stereo: far past any sane
/// period, far short of anything that strains the host.
pub const MAX_PERIOD_BYTES: u32 = 64 * 1024;
/// Largest hardware buffer the device will accept. This is the bound on the
/// host ring a guest can make us allocate: 1 MiB is ~5.4 s of 48 kHz stereo.
pub const MAX_BUFFER_BYTES: u32 = 1024 * 1024;
/// Fewest periods a buffer must hold. One period of buffer cannot be
/// double-buffered, so every real driver asks for at least two, and refusing
/// the degenerate case keeps the pump's accounting honest.
pub const MIN_PERIODS: u32 = 2;
/// Most periods a buffer may be divided into. Bounds the pending-completion
/// bookkeeping independently of the byte caps.
pub const MAX_PERIODS: u32 = 1024;

/// Sample formats the device advertises, as `VIRTIO_SND_PCM_FMT_*` values.
pub const SUPPORTED_FORMATS: &[u8] = &[FMT_S16];
/// Frame rates the device advertises, as `VIRTIO_SND_PCM_RATE_*` values.
pub const SUPPORTED_RATES: &[u8] = &[RATE_44100, RATE_48000];

/// Per-stream feature bits the device advertises: none.
///
/// `SHMEM_*` would need a shared-memory region the transports do not publish,
/// `MSG_POLLING` changes the completion contract, and both `EVT_*` bits
/// promise event-queue messages. Offering nothing keeps the guest on the
/// plain message-based path, which is the one that is implemented.
pub const ADVERTISED_PCM_FEATURES: u32 = 0;

/// Every feature bit the spec defines, so a request naming one we did not
/// advertise is distinguishable from a request naming a *reserved* bit. Both
/// are refused; the log line differs.
pub const KNOWN_PCM_FEATURES: u32 = PCM_F_SHMEM_HOST
    | PCM_F_SHMEM_GUEST
    | PCM_F_MSG_POLLING
    | PCM_F_EVT_SHMEM_PERIODS
    | PCM_F_EVT_XRUNS;

/// The `formats` bitmap published in `virtio_snd_pcm_info`.
pub fn formats_bitmap() -> u64 {
    SUPPORTED_FORMATS
        .iter()
        .fold(0u64, |bits, f| bits | (1u64 << *f))
}

/// The `rates` bitmap published in `virtio_snd_pcm_info`.
pub fn rates_bitmap() -> u64 {
    SUPPORTED_RATES
        .iter()
        .fold(0u64, |bits, r| bits | (1u64 << *r))
}

/// Why a guest request was refused, and with which virtio-snd status.
///
/// The mapping is the whole point of the type: `BAD_MSG` means "the message is
/// malformed or arrived in a state that cannot accept it", `NOT_SUPP` means
/// "well formed, but asks for something we never advertised". A driver that
/// gets the two confused debugs the wrong half of its stack.
#[derive(Debug, Clone, Copy, PartialEq, Eq, thiserror::Error)]
pub enum ParamError {
    #[error("stream id {id} is outside the advertised range 0..{STREAMS}")]
    UnknownStream { id: u32 },

    #[error("sample format {format} was never advertised")]
    Format { format: u8 },

    #[error("frame rate {rate} was never advertised")]
    Rate { rate: u8 },

    #[error("channel count {channels} outside {MIN_CHANNELS}..={MAX_CHANNELS}")]
    Channels { channels: u8 },

    #[error("stream features {requested:#x} include bits the device never advertised")]
    Features { requested: u32 },

    #[error(
        "period of {period_bytes} bytes outside {MIN_PERIOD_BYTES}..={MAX_PERIOD_BYTES} \
         or not a whole number of {frame_bytes}-byte frames"
    )]
    Period { period_bytes: u32, frame_bytes: u32 },

    #[error(
        "buffer of {buffer_bytes} bytes is not {MIN_PERIODS}..={MAX_PERIODS} whole periods \
         of {period_bytes} bytes (cap {MAX_BUFFER_BYTES})"
    )]
    Buffer {
        buffer_bytes: u32,
        period_bytes: u32,
    },

    #[error("{command} is not valid while the stream is {state:?}")]
    State {
        command: &'static str,
        state: StreamState,
    },
}

impl ParamError {
    /// The `VIRTIO_SND_S_*` code this refusal is reported with.
    pub fn status(self) -> u32 {
        match self {
            // A stream id outside what the config space advertises, and a
            // command in the wrong state, are protocol misuse: the driver
            // built a message it had the information not to build.
            Self::UnknownStream { .. } | Self::State { .. } => S_BAD_MSG,
            // Everything else is a well-formed ask for something we do not do.
            Self::Format { .. }
            | Self::Rate { .. }
            | Self::Channels { .. }
            | Self::Features { .. }
            | Self::Period { .. }
            | Self::Buffer { .. } => S_NOT_SUPP,
        }
    }
}

/// Validated PCM parameters — the only form the host side ever sees.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct PcmParams {
    /// Hardware buffer size in bytes, a whole number of periods.
    pub buffer_bytes: u32,
    /// Period size in bytes, a whole number of frames.
    pub period_bytes: u32,
    pub channels: u8,
    /// Frame rate in Hz (resolved from the `VIRTIO_SND_PCM_RATE_*` value).
    pub rate_hz: u32,
    /// The `VIRTIO_SND_PCM_FMT_*` value, kept for logging and for the day the
    /// advertised list grows.
    pub format: u8,
}

impl PcmParams {
    /// Bytes in one frame (all channels, one sample each).
    pub fn frame_bytes(&self) -> u32 {
        u32::from(self.channels) * BYTES_PER_SAMPLE
    }

    /// Periods the hardware buffer is divided into.
    pub fn periods(&self) -> u32 {
        self.buffer_bytes / self.period_bytes.max(1)
    }
}

/// Turns a guest-written `virtio_snd_pcm_set_params` into [`PcmParams`], or
/// says why not.
///
/// Order matters: identity first (so a bad stream id never reaches the
/// geometry checks), then the advertised sets, then the geometry — because the
/// frame size the geometry is validated against comes from the channel count.
pub fn validate_params(raw: &protocol::RawSetParams) -> Result<PcmParams, ParamError> {
    if raw.stream_id >= STREAMS {
        return Err(ParamError::UnknownStream { id: raw.stream_id });
    }
    if !SUPPORTED_FORMATS.contains(&raw.format) {
        return Err(ParamError::Format { format: raw.format });
    }
    if !SUPPORTED_RATES.contains(&raw.rate) {
        return Err(ParamError::Rate { rate: raw.rate });
    }
    let Some(rate_hz) = rate_hz(raw.rate) else {
        // Unreachable while SUPPORTED_RATES only holds spec values, but the
        // list is data: refuse rather than assume.
        return Err(ParamError::Rate { rate: raw.rate });
    };
    if !(MIN_CHANNELS..=MAX_CHANNELS).contains(&raw.channels) {
        return Err(ParamError::Channels {
            channels: raw.channels,
        });
    }
    if raw.features & !ADVERTISED_PCM_FEATURES != 0 {
        return Err(ParamError::Features {
            requested: raw.features,
        });
    }

    let frame_bytes = u32::from(raw.channels) * BYTES_PER_SAMPLE;
    if raw.period_bytes < MIN_PERIOD_BYTES
        || raw.period_bytes > MAX_PERIOD_BYTES
        || raw.period_bytes % frame_bytes != 0
    {
        return Err(ParamError::Period {
            period_bytes: raw.period_bytes,
            frame_bytes,
        });
    }
    // period_bytes is now known non-zero, so the division below is safe.
    let periods = raw.buffer_bytes / raw.period_bytes;
    if raw.buffer_bytes > MAX_BUFFER_BYTES
        || raw.buffer_bytes % raw.period_bytes != 0
        || !(MIN_PERIODS..=MAX_PERIODS).contains(&periods)
    {
        return Err(ParamError::Buffer {
            buffer_bytes: raw.buffer_bytes,
            period_bytes: raw.period_bytes,
        });
    }

    Ok(PcmParams {
        buffer_bytes: raw.buffer_bytes,
        period_bytes: raw.period_bytes,
        channels: raw.channels,
        rate_hz,
        format: raw.format,
    })
}

/// Why a PCM I/O message was refused.
///
/// Every variant is `VIRTIO_SND_S_BAD_MSG`: each one is the driver sending a
/// message it had the information not to send — an identifier the config space
/// never advertised, a stream that is not prepared, or a payload that
/// contradicts the geometry the driver itself negotiated. None of them is
/// "we do not support that", which is what `NOT_SUPP` means.
#[derive(Debug, Clone, Copy, PartialEq, Eq, thiserror::Error)]
pub enum XferError {
    #[error("I/O for stream id {id}, outside the advertised range 0..{STREAMS}")]
    UnknownStream { id: u32 },

    #[error("I/O for stream id {id} arrived on the queue for direction {queue}")]
    WrongDirection { id: u32, queue: u8 },

    #[error("I/O message for a stream that is {state:?}")]
    NotReady { state: StreamState },

    #[error("I/O message for a stream with no parameters")]
    NotConfigured,

    #[error(
        "payload of {len} bytes is empty, not a whole number of {frame_bytes}-byte frames, \
         or larger than the negotiated {period_bytes}-byte period"
    )]
    Payload {
        len: usize,
        frame_bytes: u32,
        period_bytes: u32,
    },
}

impl XferError {
    /// The `VIRTIO_SND_S_*` code this refusal is reported with.
    pub fn status(self) -> u32 {
        S_BAD_MSG
    }
}

/// Checks one I/O message's header and payload length against the stream the
/// driver configured. Pure, so the fuzz target can drive it directly.
///
/// `queue` is the direction of the virtqueue the message arrived on —
/// [`protocol::D_OUTPUT`] for TX, [`protocol::D_INPUT`] for RX — and a stream
/// identifier that points the other way is refused before anything else looks
/// at it. Without that check a guest could name the capture stream on the TX
/// queue and have the device treat guest-readable bytes as a buffer it may
/// fill, which is exactly the ownership confusion the RX path exists to avoid.
///
/// `payload_len` is the length the *guest* granted for this direction: the
/// device-readable audio on TX, the device-**writable** room on RX. Either way
/// it must be non-empty, a whole number of frames, and no larger than one
/// negotiated period.
///
/// Returns the parameters the caller should stage against, which is also the
/// proof that `params` was `Some` and that `payload_len` is usable.
pub fn validate_xfer(
    queue: u8,
    state: StreamState,
    params: Option<PcmParams>,
    stream_id: u32,
    payload_len: usize,
) -> Result<PcmParams, XferError> {
    let Some(direction) = direction_of(stream_id) else {
        return Err(XferError::UnknownStream { id: stream_id });
    };
    if direction != queue {
        return Err(XferError::WrongDirection {
            id: stream_id,
            queue,
        });
    }
    if !state.accepts_io() {
        return Err(XferError::NotReady { state });
    }
    let Some(params) = params else {
        return Err(XferError::NotConfigured);
    };
    let frame_bytes = params.frame_bytes();
    // `frame_bytes` is non-zero for any validated parameters (channels >= 1),
    // but the modulus is guarded rather than assumed.
    if payload_len == 0
        || frame_bytes == 0
        || payload_len % frame_bytes as usize != 0
        || payload_len > params.period_bytes as usize
    {
        return Err(XferError::Payload {
            len: payload_len,
            frame_bytes,
            period_bytes: params.period_bytes,
        });
    }
    Ok(params)
}

/// Where a PCM stream is in its lifecycle (VirtIO spec 1.2, section 5.14.6.6).
///
/// ```text
///   Unset ──SET_PARAMS──▶ ParamsSet ──PREPARE──▶ Prepared ──START──▶ Running
///     ▲                       ▲                     │  ▲                │
///     └────────RELEASE────────┴─────────────────────┘  └─────STOP───────┘
/// ```
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum StreamState {
    /// No parameters set (power-on, and where `RELEASE` returns the stream).
    #[default]
    Unset,
    /// Parameters accepted, hardware not yet claimed.
    ParamsSet,
    /// Host sink configured; the driver may queue buffers but no audio flows.
    Prepared,
    /// Audio is flowing.
    Running,
}

impl StreamState {
    /// True when the driver is allowed to put I/O messages on the TX queue.
    ///
    /// Deliberately includes `Prepared`: ALSA fills the whole hardware buffer
    /// *before* it starts the stream, and refusing those messages would make
    /// every playback begin with a burst of errors.
    pub fn accepts_io(self) -> bool {
        matches!(self, Self::Prepared | Self::Running)
    }
}

/// The stream lifecycle as a pure function, so the transitions can be tested
/// without a device, a queue or a host sink.
///
/// Returns the new state, or the refusal with its status.
pub fn transition(state: StreamState, command: u32) -> Result<StreamState, ParamError> {
    use StreamState::*;
    let name = command_name(command);
    match (command, state) {
        // PREPARE claims the host sink. Re-preparing a prepared stream is
        // legal and idempotent (the driver does it after an xrun).
        (protocol::R_PCM_PREPARE, ParamsSet | Prepared) => Ok(Prepared),
        (protocol::R_PCM_START, Prepared) => Ok(Running),
        (protocol::R_PCM_STOP, Running) => Ok(Prepared),
        // RELEASE from Running is tolerated — it implies a stop, and a driver
        // tearing down after an error should not be left with a stream it
        // cannot free. From Unset there is nothing to release.
        (protocol::R_PCM_RELEASE, ParamsSet | Prepared | Running) => Ok(Unset),
        // SET_PARAMS while audio is flowing would change the frame size under
        // the pump; everything else is fair game.
        (protocol::R_PCM_SET_PARAMS, Unset | ParamsSet | Prepared) => Ok(ParamsSet),
        (_, state) => Err(ParamError::State {
            command: name,
            state,
        }),
    }
}

/// Log label for a PCM control code.
pub fn command_name(command: u32) -> &'static str {
    match command {
        protocol::R_PCM_SET_PARAMS => "SET_PARAMS",
        protocol::R_PCM_PREPARE => "PREPARE",
        protocol::R_PCM_RELEASE => "RELEASE",
        protocol::R_PCM_START => "START",
        protocol::R_PCM_STOP => "STOP",
        protocol::R_PCM_INFO => "PCM_INFO",
        protocol::R_JACK_INFO => "JACK_INFO",
        protocol::R_JACK_REMAP => "JACK_REMAP",
        protocol::R_CHMAP_INFO => "CHMAP_INFO",
        _ => "unknown",
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::protocol::{RawSetParams, FMT_FLOAT, FMT_S32, RATE_192000, RATE_8000};

    /// `direction_of` is the only place a stream id becomes a direction, so a
    /// test that names both constants keeps the two halves honest.
    const _: () = {
        assert!(OUTPUT_STREAM < STREAMS);
        assert!(INPUT_STREAM < STREAMS);
        assert!(OUTPUT_STREAM != INPUT_STREAM);
        assert!(CHMAPS == STREAMS, "one channel map per stream");
    };

    fn good() -> RawSetParams {
        RawSetParams {
            stream_id: 0,
            buffer_bytes: 8192,
            period_bytes: 2048,
            features: 0,
            channels: 2,
            format: FMT_S16,
            rate: RATE_48000,
        }
    }

    #[test]
    fn a_well_formed_request_is_accepted_and_resolved() {
        let params = validate_params(&good()).expect("accepted");
        assert_eq!(params.rate_hz, 48000);
        assert_eq!(params.frame_bytes(), 4);
        assert_eq!(params.periods(), 4);
    }

    /// The bitmaps we publish and the sets we enforce must be the same thing:
    /// advertising a format we then refuse makes a correct driver look
    /// malicious (the lesson `virtio-block`'s discard limits wrote down).
    #[test]
    fn every_advertised_format_and_rate_is_actually_accepted() {
        for &format in SUPPORTED_FORMATS {
            assert_eq!(formats_bitmap() & (1 << format), 1 << format);
            let raw = RawSetParams { format, ..good() };
            assert!(validate_params(&raw).is_ok(), "format {format} refused");
        }
        for &rate in SUPPORTED_RATES {
            assert_eq!(rates_bitmap() & (1u64 << rate), 1u64 << rate);
            let raw = RawSetParams { rate, ..good() };
            let params = validate_params(&raw).expect("advertised rate refused");
            assert_eq!(params.rate_hz, protocol::rate_hz(rate).unwrap());
        }
    }

    /// The guest does not get to pick from a list it made up.
    #[test]
    fn formats_and_rates_we_never_advertised_are_refused_as_unsupported() {
        for format in [FMT_S32, FMT_FLOAT, 0, protocol::FMT_COUNT, 255] {
            let error = validate_params(&RawSetParams { format, ..good() })
                .expect_err("unadvertised format accepted");
            assert_eq!(error, ParamError::Format { format });
            assert_eq!(error.status(), S_NOT_SUPP);
        }
        for rate in [RATE_8000, RATE_192000, 0, protocol::RATE_COUNT, 255] {
            let error =
                validate_params(&RawSetParams { rate, ..good() }).expect_err("rate accepted");
            assert_eq!(error, ParamError::Rate { rate });
            assert_eq!(error.status(), S_NOT_SUPP);
        }
    }

    /// Two streams, and the two identifiers point opposite ways. Everything
    /// the RX path does is keyed off this being a device constant.
    #[test]
    fn the_two_advertised_streams_are_one_out_and_one_in() {
        assert_eq!(STREAMS, 2);
        assert_eq!(direction_of(OUTPUT_STREAM), Some(D_OUTPUT));
        assert_eq!(direction_of(INPUT_STREAM), Some(D_INPUT));
        for id in [STREAMS, STREAMS + 1, u32::MAX] {
            assert_eq!(direction_of(id), None, "stream {id} is not advertised");
        }
    }

    /// Both directions take the same parameters: the guest's own stack does
    /// the converting, and a capture stream the playback stream cannot mirror
    /// would need a second host code path on an untrusted input.
    #[test]
    fn both_streams_accept_exactly_the_same_geometry() {
        for stream_id in [OUTPUT_STREAM, INPUT_STREAM] {
            let params = validate_params(&RawSetParams {
                stream_id,
                ..good()
            })
            .expect("both directions take the advertised set");
            assert_eq!(params.rate_hz, 48000);
            assert_eq!(params.frame_bytes(), 4);
        }
    }

    #[test]
    fn the_stream_id_must_be_one_the_config_space_advertises() {
        for stream_id in [STREAMS, STREAMS + 1, u32::MAX] {
            let error = validate_params(&RawSetParams {
                stream_id,
                ..good()
            })
            .expect_err("unknown stream accepted");
            assert_eq!(error, ParamError::UnknownStream { id: stream_id });
            assert_eq!(error.status(), S_BAD_MSG);
        }
    }

    #[test]
    fn channel_counts_are_bounded_by_what_the_chmap_describes() {
        for channels in [0u8, MAX_CHANNELS + 1, 18, 255] {
            let error = validate_params(&RawSetParams { channels, ..good() })
                .expect_err("channel count accepted");
            assert_eq!(error, ParamError::Channels { channels });
        }
    }

    #[test]
    fn unadvertised_stream_features_are_refused() {
        for requested in [
            PCM_F_SHMEM_HOST,
            PCM_F_MSG_POLLING,
            PCM_F_EVT_XRUNS,
            !KNOWN_PCM_FEATURES,
            u32::MAX,
        ] {
            let error = validate_params(&RawSetParams {
                features: requested,
                ..good()
            })
            .expect_err("feature accepted");
            assert_eq!(error, ParamError::Features { requested });
        }
    }

    /// The bound that matters most: a guest must not be able to name a period
    /// or buffer size that makes the host allocate without limit, and must not
    /// be able to name zero and divide by it.
    #[test]
    fn period_and_buffer_geometry_is_bounded_and_never_divides_by_zero() {
        // Zero, sub-minimum, over-cap and non-frame-aligned periods.
        for period_bytes in [0, 1, MIN_PERIOD_BYTES - 1, MAX_PERIOD_BYTES + 1, u32::MAX] {
            let raw = RawSetParams {
                period_bytes,
                buffer_bytes: period_bytes.saturating_mul(2),
                ..good()
            };
            assert!(
                matches!(
                    validate_params(&raw),
                    Err(ParamError::Period { .. }) | Err(ParamError::Buffer { .. })
                ),
                "period {period_bytes} accepted"
            );
        }
        // 2050 is not a multiple of the 4-byte stereo frame.
        assert_eq!(
            validate_params(&RawSetParams {
                period_bytes: 2050,
                buffer_bytes: 4100,
                ..good()
            }),
            Err(ParamError::Period {
                period_bytes: 2050,
                frame_bytes: 4,
            })
        );
        // Buffers: zero, one period, a non-multiple, and past the cap.
        for buffer_bytes in [
            0,
            2048,
            3000,
            MAX_BUFFER_BYTES + 2048,
            MAX_PERIODS * 2048 + 2048,
            u32::MAX,
        ] {
            let raw = RawSetParams {
                buffer_bytes,
                ..good()
            };
            assert!(
                matches!(validate_params(&raw), Err(ParamError::Buffer { .. })),
                "buffer {buffer_bytes} accepted"
            );
        }
        // And the largest thing we *do* accept really is bounded by the cap.
        let raw = RawSetParams {
            period_bytes: MAX_PERIOD_BYTES,
            buffer_bytes: MAX_BUFFER_BYTES,
            ..good()
        };
        let params = validate_params(&raw).expect("the caps themselves are acceptable");
        assert_eq!(params.periods(), MAX_BUFFER_BYTES / MAX_PERIOD_BYTES);
    }

    #[test]
    fn the_lifecycle_only_walks_the_edges_the_spec_draws() {
        use StreamState::*;
        assert_eq!(transition(Unset, protocol::R_PCM_SET_PARAMS), Ok(ParamsSet));
        assert_eq!(transition(ParamsSet, protocol::R_PCM_PREPARE), Ok(Prepared));
        assert_eq!(transition(Prepared, protocol::R_PCM_PREPARE), Ok(Prepared));
        assert_eq!(transition(Prepared, protocol::R_PCM_START), Ok(Running));
        assert_eq!(transition(Running, protocol::R_PCM_STOP), Ok(Prepared));
        assert_eq!(transition(Prepared, protocol::R_PCM_RELEASE), Ok(Unset));
        assert_eq!(transition(Running, protocol::R_PCM_RELEASE), Ok(Unset));

        // And refuses the ones it must, with BAD_MSG rather than a panic.
        for (state, command) in [
            (Unset, protocol::R_PCM_PREPARE),
            (Unset, protocol::R_PCM_START),
            (Unset, protocol::R_PCM_STOP),
            (Unset, protocol::R_PCM_RELEASE),
            (ParamsSet, protocol::R_PCM_START),
            (ParamsSet, protocol::R_PCM_STOP),
            (Prepared, protocol::R_PCM_STOP),
            (Running, protocol::R_PCM_START),
            (Running, protocol::R_PCM_PREPARE),
            (Running, protocol::R_PCM_SET_PARAMS),
        ] {
            let error = transition(state, command).expect_err("illegal transition allowed");
            assert_eq!(error.status(), S_BAD_MSG, "{state:?} + {command:#x}");
        }
    }

    /// The I/O gate: every refusal is BAD_MSG, and an accepted payload really
    /// is a whole number of frames inside one period.
    #[test]
    fn io_messages_are_checked_against_the_geometry_the_driver_negotiated() {
        let params = validate_params(&good()).expect("accepted");
        let frame = params.frame_bytes() as usize;

        assert_eq!(
            validate_xfer(
                D_OUTPUT,
                StreamState::Prepared,
                Some(params),
                OUTPUT_STREAM,
                frame
            ),
            Ok(params),
            "a prepared stream takes buffers: ALSA fills before it starts"
        );
        assert_eq!(
            validate_xfer(
                D_OUTPUT,
                StreamState::Running,
                Some(params),
                OUTPUT_STREAM,
                params.period_bytes as usize
            ),
            Ok(params)
        );
        // And the same rules hold for the capture stream on the capture queue,
        // where the length being checked is device-*writable* room.
        assert_eq!(
            validate_xfer(
                D_INPUT,
                StreamState::Running,
                Some(params),
                INPUT_STREAM,
                params.period_bytes as usize
            ),
            Ok(params)
        );

        for (queue, state, params_in, id, len, expected) in [
            (
                D_OUTPUT,
                StreamState::Running,
                Some(params),
                STREAMS,
                frame,
                XferError::UnknownStream { id: STREAMS },
            ),
            (
                D_OUTPUT,
                StreamState::Unset,
                Some(params),
                OUTPUT_STREAM,
                frame,
                XferError::NotReady {
                    state: StreamState::Unset,
                },
            ),
            (
                D_OUTPUT,
                StreamState::ParamsSet,
                Some(params),
                OUTPUT_STREAM,
                frame,
                XferError::NotReady {
                    state: StreamState::ParamsSet,
                },
            ),
            (
                D_OUTPUT,
                StreamState::Running,
                None,
                OUTPUT_STREAM,
                frame,
                XferError::NotConfigured,
            ),
            // The ownership inversion in one line: the capture stream named on
            // the playback queue, and the playback stream on the capture one.
            (
                D_OUTPUT,
                StreamState::Running,
                Some(params),
                INPUT_STREAM,
                frame,
                XferError::WrongDirection {
                    id: INPUT_STREAM,
                    queue: D_OUTPUT,
                },
            ),
            (
                D_INPUT,
                StreamState::Running,
                Some(params),
                OUTPUT_STREAM,
                frame,
                XferError::WrongDirection {
                    id: OUTPUT_STREAM,
                    queue: D_INPUT,
                },
            ),
        ] {
            assert_eq!(
                validate_xfer(queue, state, params_in, id, len),
                Err(expected),
                "{queue} {state:?} {id} {len}"
            );
            assert_eq!(expected.status(), S_BAD_MSG);
        }

        // Empty, partial and oversized payloads, in both directions.
        for (queue, id) in [(D_OUTPUT, OUTPUT_STREAM), (D_INPUT, INPUT_STREAM)] {
            for len in [
                0,
                1,
                frame + 1,
                params.period_bytes as usize + frame,
                usize::MAX,
            ] {
                assert!(
                    matches!(
                        validate_xfer(queue, StreamState::Running, Some(params), id, len),
                        Err(XferError::Payload { .. })
                    ),
                    "payload of {len} bytes accepted on queue {queue}"
                );
            }
        }
    }

    #[test]
    fn only_prepared_and_running_streams_take_io_messages() {
        assert!(!StreamState::Unset.accepts_io());
        assert!(!StreamState::ParamsSet.accepts_io());
        assert!(StreamState::Prepared.accepts_io());
        assert!(StreamState::Running.accepts_io());
    }
}
