//! Fuzzes the isolated-renderer wire format (ADR-0004's GPU-012 amendment).
//!
//! Both directions matter, for different reasons:
//!
//! * a **reply** is what the VMM reads from the helper — the process that runs
//!   guest-derived GL work and is expected to crash. A hostile or corrupted
//!   helper must not be able to panic the VMM or make it allocate on a promise
//!   the payload cannot keep, because that would defeat the containment.
//! * a **request** is what the helper reads from the VMM. A malformed frame
//!   must not crash the helper either, or the bug would look exactly like the
//!   renderer crash this design exists to survive.
//!
//! The target drives the framing *and* the codec: arbitrary bytes go through
//! `read_frame` (length header included), and every tag is decoded directly so
//! that tag values the stream never happened to produce are exercised too.
//!
//! Properties: no panic, no abort, nothing buffered past `MAX_FRAME_BYTES`,
//! and anything that decodes must re-encode to the same bytes — a codec whose
//! halves disagree would silently corrupt commands across the process
//! boundary.

#![no_main]

use libfuzzer_sys::fuzz_target;
use virtio_gpu::remote::protocol::{Reply, Request};
use virtio_gpu::remote::{read_frame, MAX_FRAME_BYTES};

fuzz_target!(|data: &[u8]| {
    // Layer 1: the framing, over a stream of arbitrary bytes. Several frames
    // may be in there; read until the reader is unhappy.
    let mut cursor = std::io::Cursor::new(data);
    let mut payload = Vec::new();
    while let Ok(tag) = read_frame(&mut cursor, &mut payload) {
        assert!(payload.len() <= MAX_FRAME_BYTES);
        let _ = Request::decode(tag, &payload);
        let _ = Reply::decode(tag, &payload);
    }

    // Layer 2: every tag against the raw bytes, with an exact re-encode check.
    for tag in 0u8..=0x90 {
        if let Ok(request) = Request::decode(tag, data) {
            assert_eq!(request.tag(), tag);
            assert_eq!(request.encode(), data, "request re-encode must be exact");
        }
        if let Ok(reply) = Reply::decode(tag, data) {
            assert_eq!(reply.tag(), tag);
            assert_eq!(reply.encode(), data, "reply re-encode must be exact");
        }
    }
});
