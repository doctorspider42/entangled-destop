//! Time for the guest: an SNTP server answering from the host's clock
//! (2026-10-12).
//!
//! The NAT carries no UDP beyond DHCP and DNS, so a guest's NTP client never
//! reached a server: `timedatectl` said "System clock synchronized: no", and the
//! guest kept whatever its RTC said at boot, drifting from there. Every NTP
//! request the guest sends — to whichever server it was configured with, on
//! port 123 — is now answered by this process, from the host's wall clock,
//! with the reply sourced from the address the guest asked. The host's clock is
//! what the guest should agree with anyway; it is the host's job to keep it
//! right.
//!
//! **The guest is untrusted:** a request is 48 bytes or more, mode 3 (client),
//! version 1 to 4, and the only thing copied out of it is its transmit
//! timestamp, into the reply's origin field — which is what lets the client
//! match the answer to its question.

use std::time::{SystemTime, UNIX_EPOCH};

/// The NTP port.
pub const PORT: u16 = 123;
/// Length of an NTP header without extensions.
pub const LEN: usize = 48;
/// Seconds from the NTP epoch (1900) to the Unix one (1970).
const NTP_UNIX_OFFSET: u64 = 2_208_988_800;
/// What this server says it is: stratum 2, the host's clock being its source.
const STRATUM: u8 = 2;
/// Reference id for a stratum-2 server is its source's IPv4 address; this is
/// 127.127.1.0, the conventional "local clock".
const REFERENCE_ID: [u8; 4] = [127, 127, 1, 0];

/// `now` as a 64-bit NTP timestamp: seconds since 1900 and a 32-bit fraction.
pub fn timestamp(now: SystemTime) -> [u8; 8] {
    let since = now.duration_since(UNIX_EPOCH).unwrap_or_default();
    let seconds = (since.as_secs() + NTP_UNIX_OFFSET) as u32;
    let fraction = ((u64::from(since.subsec_nanos()) << 32) / 1_000_000_000) as u32;
    let mut out = [0u8; 8];
    out[..4].copy_from_slice(&seconds.to_be_bytes());
    out[4..].copy_from_slice(&fraction.to_be_bytes());
    out
}

/// The server's reply to `request` at `now`, or `None` if it is not a client
/// request this answers.
pub fn reply(request: &[u8], now: SystemTime) -> Option<[u8; LEN]> {
    let header = request.get(..LEN)?;
    let version = (header[0] >> 3) & 0x7;
    let mode = header[0] & 0x7;
    if mode != 3 || !(1..=4).contains(&version) {
        return None;
    }
    let mut out = [0u8; LEN];
    // Leap indicator 0 (no warning), the client's version, mode 4 (server).
    out[0] = (version << 3) | 4;
    out[1] = STRATUM;
    out[2] = header[2]; // poll: echo the client's
    out[3] = (-20i8) as u8; // precision: about a microsecond
                            // Root delay 0; root dispersion about a millisecond (16.16 seconds).
    out[8..12].copy_from_slice(&0x0000_0042u32.to_be_bytes());
    out[12..16].copy_from_slice(&REFERENCE_ID);
    let ts = timestamp(now);
    out[16..24].copy_from_slice(&ts); // reference: the clock is live
    out[24..32].copy_from_slice(&header[40..48]); // origin: the client's transmit
    out[32..40].copy_from_slice(&ts); // receive
    out[40..48].copy_from_slice(&ts); // transmit
    Some(out)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::Duration;

    fn request(version: u8, mode: u8) -> [u8; LEN] {
        let mut r = [0u8; LEN];
        r[0] = (version << 3) | mode;
        r[2] = 6;
        r[40..48].copy_from_slice(&[1, 2, 3, 4, 5, 6, 7, 8]);
        r
    }

    #[test]
    fn a_client_request_gets_a_server_reply_with_its_origin_echoed() {
        let now = UNIX_EPOCH + Duration::from_secs(1_760_000_000) + Duration::from_millis(500);
        let r = reply(&request(4, 3), now).expect("a reply");
        assert_eq!(r[0], (4 << 3) | 4, "v4, server");
        assert_eq!(r[1], STRATUM);
        assert_eq!(r[2], 6, "poll echoed");
        assert_eq!(
            &r[24..32],
            &[1, 2, 3, 4, 5, 6, 7, 8],
            "origin = client's transmit"
        );
        let seconds = u32::from_be_bytes([r[40], r[41], r[42], r[43]]);
        assert_eq!(u64::from(seconds), 1_760_000_000 + NTP_UNIX_OFFSET);
        let fraction = u32::from_be_bytes([r[44], r[45], r[46], r[47]]);
        assert!(
            (fraction as f64 / 4_294_967_296.0 - 0.5).abs() < 1e-6,
            "half a second"
        );
    }

    #[test]
    fn anything_but_a_client_request_is_ignored() {
        let now = SystemTime::now();
        assert!(reply(&request(4, 4), now).is_none(), "a server packet");
        assert!(reply(&request(0, 3), now).is_none(), "version 0");
        assert!(reply(&request(5, 3), now).is_none(), "version 5");
        assert!(reply(&request(4, 3)[..47], now).is_none(), "short");
        assert!(reply(&request(3, 3), now).is_some(), "SNTPv3 is fine");
    }
}
