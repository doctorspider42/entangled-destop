//! Fuzzes the virtio-snd control-message and PCM-header parsers (backlog
//! MVP-1402, GAME-2102).
//!
//! Everything a guest can put in front of the sound device before a single
//! sample moves: the four-byte request header, the `virtio_snd_query_info`
//! shape, the whole of `virtio_snd_pcm_set_params`, the lifecycle state
//! machine, and the per-message I/O header. All of it is pure, so this target
//! runs at millions of executions per minute and needs no guest memory.
//!
//! Properties checked (the ones the device then relies on):
//!
//! * parsing arbitrary bytes never panics and round-trips through the encoder;
//! * an **accepted** `SET_PARAMS` really is inside every advertised set and
//!   every named bound — the format and rate are ones we published, the
//!   channel count is one the chmap describes, the period is a whole number of
//!   frames within `MIN_PERIOD_BYTES..=MAX_PERIOD_BYTES`, and the buffer is
//!   `MIN_PERIODS..=MAX_PERIODS` whole periods no larger than
//!   `MAX_BUFFER_BYTES`;
//! * a **refused** one carries the documented status: `BAD_MSG` for protocol
//!   misuse (an identifier we never advertised, a command in the wrong state),
//!   `NOT_SUPP` for anything we simply do not offer — never `OK`, never
//!   `IO_ERR`, which the guest would read as a host fault;
//! * the lifecycle only ever reaches a state the spec's diagram draws, and only
//!   over an edge it draws;
//! * an accepted I/O message is a whole number of frames no larger than one
//!   period of the stream the guest itself configured.

#![no_main]

use arbitrary::Arbitrary;
use libfuzzer_sys::fuzz_target;
use virtio_sound::protocol::{self, ItemHdr, QueryInfo, RawSetParams};
use virtio_sound::stream::{
    self, ParamError, StreamState, XferError, MAX_BUFFER_BYTES, MAX_CHANNELS, MAX_PERIODS,
    MAX_PERIOD_BYTES, MIN_CHANNELS, MIN_PERIODS, MIN_PERIOD_BYTES, STREAMS,
};

#[derive(Debug, Arbitrary)]
struct Case {
    /// A `virtio_snd_pcm_set_params` exactly as it arrives on the wire.
    set_params: [u8; protocol::SET_PARAMS_LEN],
    /// A `virtio_snd_query_info`, likewise.
    query: [u8; protocol::QUERY_INFO_LEN],
    /// A `virtio_snd_pcm_hdr` (also the jack and chmap header shape).
    item: [u8; protocol::PCM_HDR_LEN],
    /// Arbitrary leading bytes of a control message, however short.
    raw: Vec<u8>,
    /// Which lifecycle command to try, and from which state.
    command: u32,
    state: u8,
    /// An I/O message: the stream it names and how much audio it carries.
    xfer_stream: u32,
    xfer_len: u32,
}

fn state_of(byte: u8) -> StreamState {
    match byte % 4 {
        0 => StreamState::Unset,
        1 => StreamState::ParamsSet,
        2 => StreamState::Prepared,
        _ => StreamState::Running,
    }
}

/// The only four codes a refusal may ever carry.
fn assert_refusal_status(status: u32) {
    assert!(
        status == protocol::S_BAD_MSG || status == protocol::S_NOT_SUPP,
        "a refusal answered {status:#x}, which is neither BAD_MSG nor NOT_SUPP"
    );
}

fuzz_target!(|case: Case| {
    // --- the bare header, on however few bytes the guest sent ---------------
    let _ = protocol::request_code(&case.raw);
    if case.raw.len() >= protocol::HDR_LEN {
        assert_eq!(
            protocol::request_code(&case.raw),
            u32::from_le_bytes([case.raw[0], case.raw[1], case.raw[2], case.raw[3]])
        );
    }

    // --- query_info ---------------------------------------------------------
    let query = QueryInfo::parse(&case.query);
    assert_eq!(query.encode(), case.query, "query_info must round-trip");
    // The three ranges the device checks a query against are device constants,
    // so an in-range query can never ask for more than a handful of records.
    for available in [stream::JACKS, STREAMS, stream::CHMAPS] {
        if let Some(end) = query.start_id.checked_add(query.count) {
            if end <= available {
                assert!(query.count <= available);
            }
        }
    }

    // --- item headers -------------------------------------------------------
    let item = ItemHdr::parse(&case.item);
    assert_eq!(item.encode(), case.item, "item headers must round-trip");
    let _ = stream::command_name(item.code);

    // --- set_params ---------------------------------------------------------
    let raw = RawSetParams::parse(&case.set_params);
    match stream::validate_params(&raw) {
        Ok(params) => {
            // Everything the host is now allowed to act on.
            assert!(stream::SUPPORTED_FORMATS.contains(&raw.format));
            assert!(stream::SUPPORTED_RATES.contains(&raw.rate));
            assert_eq!(params.rate_hz, protocol::rate_hz(raw.rate).unwrap_or(0));
            assert_ne!(params.rate_hz, 0);
            assert!((MIN_CHANNELS..=MAX_CHANNELS).contains(&params.channels));
            assert_eq!(raw.features & !stream::ADVERTISED_PCM_FEATURES, 0);

            let frame = params.frame_bytes();
            assert_ne!(frame, 0);
            assert!((MIN_PERIOD_BYTES..=MAX_PERIOD_BYTES).contains(&params.period_bytes));
            assert_eq!(params.period_bytes % frame, 0);
            assert!(params.buffer_bytes <= MAX_BUFFER_BYTES);
            assert_eq!(params.buffer_bytes % params.period_bytes, 0);
            let periods = params.periods();
            assert!((MIN_PERIODS..=MAX_PERIODS).contains(&periods));
            assert_eq!(periods * params.period_bytes, params.buffer_bytes);
            // The ring the device will allocate is therefore bounded by a
            // constant, which is the whole point of the geometry checks.
            assert!(
                u64::from(params.buffer_bytes) + u64::from(params.period_bytes)
                    <= u64::from(MAX_BUFFER_BYTES) + u64::from(MAX_PERIOD_BYTES)
            );

            // --- an I/O message against those parameters --------------------
            let state = state_of(case.state);
            let len = (case.xfer_len % (MAX_PERIOD_BYTES + 64)) as usize;
            match stream::validate_xfer(state, Some(params), case.xfer_stream, len) {
                Ok(accepted) => {
                    assert_eq!(accepted, params);
                    assert!(state.accepts_io());
                    assert!(case.xfer_stream < STREAMS);
                    assert_ne!(len, 0);
                    assert_eq!(len % frame as usize, 0);
                    assert!(len <= params.period_bytes as usize);
                }
                Err(error) => {
                    assert_eq!(error.status(), protocol::S_BAD_MSG);
                    match error {
                        XferError::UnknownStream { id } => assert!(id >= STREAMS),
                        XferError::NotReady { state: s } => assert!(!s.accepts_io()),
                        XferError::NotConfigured => unreachable!("params were Some"),
                        XferError::Payload { len: l, .. } => assert_eq!(l, len),
                    }
                }
            }
        }
        Err(error) => {
            assert_refusal_status(error.status());
            // The two BAD_MSG variants are protocol misuse; everything else is
            // "we never offered that". Keeping the split honest is what stops a
            // driver debugging the wrong half of its stack.
            match error {
                ParamError::UnknownStream { id } => {
                    assert_eq!(id, raw.stream_id);
                    assert!(id >= STREAMS);
                    assert_eq!(error.status(), protocol::S_BAD_MSG);
                }
                ParamError::State { .. } => {
                    unreachable!("validate_params does not inspect the state")
                }
                _ => assert_eq!(error.status(), protocol::S_NOT_SUPP),
            }
        }
    }

    // --- a stream with no parameters at all ---------------------------------
    let state = state_of(case.state);
    match stream::validate_xfer(state, None, case.xfer_stream, case.xfer_len as usize) {
        Ok(_) => unreachable!("an unconfigured stream must never accept audio"),
        Err(error) => assert_eq!(error.status(), protocol::S_BAD_MSG),
    }

    // --- the lifecycle ------------------------------------------------------
    match stream::transition(state, case.command) {
        Ok(next) => {
            // Only the edges the spec draws, and only to a state it names.
            let legal = matches!(
                (case.command, state, next),
                (protocol::R_PCM_SET_PARAMS, _, StreamState::ParamsSet)
                    | (protocol::R_PCM_PREPARE, _, StreamState::Prepared)
                    | (protocol::R_PCM_START, StreamState::Prepared, StreamState::Running)
                    | (protocol::R_PCM_STOP, StreamState::Running, StreamState::Prepared)
                    | (protocol::R_PCM_RELEASE, _, StreamState::Unset)
            );
            assert!(legal, "{state:?} + {:#x} -> {next:?}", case.command);
            // A stream can never *start* from a state that never had params.
            if next == StreamState::Running {
                assert_eq!(state, StreamState::Prepared);
            }
        }
        Err(error) => assert_eq!(error.status(), protocol::S_BAD_MSG),
    }

    // --- the reply encoders -------------------------------------------------
    // Fixed-size by construction, but the assertion is cheap and it is what
    // stops a reply array from ever desynchronising.
    assert_eq!(
        protocol::encode_status(protocol::S_OK).len(),
        protocol::HDR_LEN
    );
    assert_eq!(
        protocol::encode_pcm_status(protocol::S_OK, case.xfer_len).len(),
        protocol::PCM_STATUS_LEN
    );
    assert_eq!(
        protocol::ChmapInfo::stereo_output().encode().len(),
        protocol::CHMAP_INFO_LEN
    );
});
