//! The virtio-snd wire format (VirtIO spec 1.2, section 5.14).
//!
//! Constants and fixed-size structs only: every parser takes a byte array of
//! the exact struct length and every encoder returns one, so nothing in this
//! module can index out of bounds or depend on host struct layout. The lengths
//! themselves are asserted against the spec at compile time at the bottom of
//! the file — the same trick `virtio_gpu::protocol` uses, and the reason a
//! typo in a field offset is a build failure rather than a guest that hears
//! static.
//!
//! Nothing here allocates, touches guest memory or knows what a virtqueue is;
//! `device.rs` owns all of that. That split is what lets the fuzz targets
//! drive the parsers directly.

// ------------------------------------------------------------- virtqueues

/// Control queue: information queries and the PCM stream lifecycle.
pub const VQ_CONTROL: u16 = 0;
/// Event queue: device-to-driver notifications. We offer none of the features
/// that produce events, so the guest's buffers sit here unused (see
/// [`crate::device`]).
pub const VQ_EVENT: u16 = 1;
/// Playback queue (guest → host).
pub const VQ_TX: u16 = 2;
/// Capture queue (host → guest). The guest posts *empty* buffers here and the
/// device fills them — the ownership inversion the RX path is built around
/// (see [`crate::device`]).
pub const VQ_RX: u16 = 3;
/// Number of virtqueues a virtio-snd device exposes. Fixed by the spec.
pub const NUM_QUEUES: usize = 4;

// ---------------------------------------------------------------- dataflow

/// `VIRTIO_SND_D_OUTPUT`: playback.
pub const D_OUTPUT: u8 = 0;
/// `VIRTIO_SND_D_INPUT`: capture.
pub const D_INPUT: u8 = 1;

// ----------------------------------------------------------- request codes

/// `VIRTIO_SND_R_JACK_INFO`.
pub const R_JACK_INFO: u32 = 1;
/// `VIRTIO_SND_R_JACK_REMAP`.
pub const R_JACK_REMAP: u32 = 2;
/// `VIRTIO_SND_R_PCM_INFO`.
pub const R_PCM_INFO: u32 = 0x0100;
/// `VIRTIO_SND_R_PCM_SET_PARAMS`.
pub const R_PCM_SET_PARAMS: u32 = 0x0101;
/// `VIRTIO_SND_R_PCM_PREPARE`.
pub const R_PCM_PREPARE: u32 = 0x0102;
/// `VIRTIO_SND_R_PCM_RELEASE`.
pub const R_PCM_RELEASE: u32 = 0x0103;
/// `VIRTIO_SND_R_PCM_START`.
pub const R_PCM_START: u32 = 0x0104;
/// `VIRTIO_SND_R_PCM_STOP`.
pub const R_PCM_STOP: u32 = 0x0105;
/// `VIRTIO_SND_R_CHMAP_INFO`.
pub const R_CHMAP_INFO: u32 = 0x0200;

/// `VIRTIO_SND_EVT_PCM_PERIOD_ELAPSED`.
pub const EVT_PCM_PERIOD_ELAPSED: u32 = 0x1100;
/// `VIRTIO_SND_EVT_PCM_XRUN`.
pub const EVT_PCM_XRUN: u32 = 0x1101;

// -------------------------------------------------------------- status codes

/// `VIRTIO_SND_S_OK`.
pub const S_OK: u32 = 0x8000;
/// `VIRTIO_SND_S_BAD_MSG`: the message itself is malformed — wrong length,
/// wrong descriptor shape, an identifier outside the advertised range, or a
/// lifecycle command in a state that cannot accept it.
pub const S_BAD_MSG: u32 = 0x8001;
/// `VIRTIO_SND_S_NOT_SUPP`: the message is well formed but asks for something
/// this device does not offer — an unadvertised format, rate, channel count or
/// stream feature, or a buffer geometry past our caps.
pub const S_NOT_SUPP: u32 = 0x8002;
/// `VIRTIO_SND_S_IO_ERR`: the host side failed.
pub const S_IO_ERR: u32 = 0x8003;

// ---------------------------------------------------------- struct lengths

/// `struct virtio_snd_hdr`.
pub const HDR_LEN: usize = 4;
/// `struct virtio_snd_event`.
pub const EVENT_LEN: usize = 8;
/// `struct virtio_snd_query_info`.
pub const QUERY_INFO_LEN: usize = 16;
/// `struct virtio_snd_jack_hdr`.
pub const JACK_HDR_LEN: usize = 8;
/// `struct virtio_snd_jack_info`.
pub const JACK_INFO_LEN: usize = 24;
/// `struct virtio_snd_jack_remap`.
pub const JACK_REMAP_LEN: usize = 16;
/// `struct virtio_snd_pcm_hdr`.
pub const PCM_HDR_LEN: usize = 8;
/// `struct virtio_snd_pcm_info`.
pub const PCM_INFO_LEN: usize = 32;
/// `struct virtio_snd_pcm_set_params`.
pub const SET_PARAMS_LEN: usize = 24;
/// `struct virtio_snd_pcm_xfer`.
pub const PCM_XFER_LEN: usize = 4;
/// `struct virtio_snd_pcm_status`.
pub const PCM_STATUS_LEN: usize = 8;
/// `struct virtio_snd_chmap_hdr`.
pub const CHMAP_HDR_LEN: usize = 8;
/// `struct virtio_snd_chmap_info`.
pub const CHMAP_INFO_LEN: usize = 24;
/// `VIRTIO_SND_CHMAP_MAX_SIZE`.
pub const CHMAP_MAX_SIZE: usize = 18;

// -------------------------------------------------------- sample formats

/// `VIRTIO_SND_PCM_FMT_S16` — the one format this device advertises. See
/// [`crate::stream`] for why the list is deliberately short.
pub const FMT_S16: u8 = 5;
/// `VIRTIO_SND_PCM_FMT_S32`, named so the "we refuse what we did not
/// advertise" tests have a plausible neighbour to ask for.
pub const FMT_S32: u8 = 17;
/// `VIRTIO_SND_PCM_FMT_FLOAT`.
pub const FMT_FLOAT: u8 = 19;
/// One past the last format the spec defines (`IEC958_SUBFRAME` = 24).
pub const FMT_COUNT: u8 = 25;

// ---------------------------------------------------------- frame rates

/// `VIRTIO_SND_PCM_RATE_8000`.
pub const RATE_8000: u8 = 1;
/// `VIRTIO_SND_PCM_RATE_44100`.
pub const RATE_44100: u8 = 6;
/// `VIRTIO_SND_PCM_RATE_48000`.
pub const RATE_48000: u8 = 7;
/// `VIRTIO_SND_PCM_RATE_192000`.
pub const RATE_192000: u8 = 12;
/// One past the last rate the spec defines (`384000` = 13).
pub const RATE_COUNT: u8 = 14;

/// Frame rate in Hz for a `VIRTIO_SND_PCM_RATE_*` value, or `None` when the
/// guest named something outside the enum.
pub const fn rate_hz(rate: u8) -> Option<u32> {
    Some(match rate {
        0 => 5512,
        1 => 8000,
        2 => 11025,
        3 => 16000,
        4 => 22050,
        5 => 32000,
        6 => 44100,
        7 => 48000,
        8 => 64000,
        9 => 88200,
        10 => 96000,
        11 => 176_400,
        12 => 192_000,
        13 => 384_000,
        _ => return None,
    })
}

// ------------------------------------------------------ channel positions

/// `VIRTIO_SND_CHMAP_NONE`.
pub const CHMAP_NONE: u8 = 0;
/// `VIRTIO_SND_CHMAP_MONO`.
pub const CHMAP_MONO: u8 = 2;
/// `VIRTIO_SND_CHMAP_FL`.
pub const CHMAP_FL: u8 = 3;
/// `VIRTIO_SND_CHMAP_FR`.
pub const CHMAP_FR: u8 = 4;

// ------------------------------------------------------- HDA pin defaults
//
// `hda_reg_defconf` is an HDA pin default-configuration dword, which is how a
// virtio-snd jack says what it physically is. Only two fields matter to us:
// bits 20..23 are the *default device* and bits 30..31 the port connectivity.
// Everything else (colour, location, association) stays zero, which reads as
// "unknown", and a driver that shows a name gets one from the device field.

/// Bit position of the HDA "default device" field.
const HDA_DEFCONF_DEVICE_SHIFT: u32 = 20;
/// HDA default device `Line Out`.
const HDA_DEVICE_LINE_OUT: u32 = 0x0;
/// HDA default device `Mic In`.
const HDA_DEVICE_MIC_IN: u32 = 0xa;

/// `hda_reg_defconf` for the line-out jack.
pub const DEFCONF_LINE_OUT: u32 = HDA_DEVICE_LINE_OUT << HDA_DEFCONF_DEVICE_SHIFT;
/// `hda_reg_defconf` for the microphone jack.
pub const DEFCONF_MIC_IN: u32 = HDA_DEVICE_MIC_IN << HDA_DEFCONF_DEVICE_SHIFT;

// ------------------------------------------------------- PCM stream features

/// `VIRTIO_SND_PCM_F_SHMEM_HOST`.
pub const PCM_F_SHMEM_HOST: u32 = 1 << 0;
/// `VIRTIO_SND_PCM_F_SHMEM_GUEST`.
pub const PCM_F_SHMEM_GUEST: u32 = 1 << 1;
/// `VIRTIO_SND_PCM_F_MSG_POLLING`.
pub const PCM_F_MSG_POLLING: u32 = 1 << 2;
/// `VIRTIO_SND_PCM_F_EVT_SHMEM_PERIODS`.
pub const PCM_F_EVT_SHMEM_PERIODS: u32 = 1 << 3;
/// `VIRTIO_SND_PCM_F_EVT_XRUNS`.
pub const PCM_F_EVT_XRUNS: u32 = 1 << 4;

// ------------------------------------------------------------- little helpers

fn le32(raw: &[u8], at: usize) -> u32 {
    let mut buf = [0u8; 4];
    if let Some(slice) = raw.get(at..at + 4) {
        buf.copy_from_slice(slice);
    }
    u32::from_le_bytes(buf)
}

fn put32(raw: &mut [u8], at: usize, value: u32) {
    if let Some(slice) = raw.get_mut(at..at + 4) {
        slice.copy_from_slice(&value.to_le_bytes());
    }
}

fn put64(raw: &mut [u8], at: usize, value: u64) {
    if let Some(slice) = raw.get_mut(at..at + 8) {
        slice.copy_from_slice(&value.to_le_bytes());
    }
}

/// The four-byte `struct virtio_snd_hdr` every control request starts with.
pub fn request_code(raw: &[u8]) -> u32 {
    le32(raw, 0)
}

/// Encodes a bare `struct virtio_snd_hdr` (a status-only response).
pub fn encode_status(status: u32) -> [u8; HDR_LEN] {
    status.to_le_bytes()
}

// ------------------------------------------------------------ query requests

/// `struct virtio_snd_query_info` — the shape of every `*_INFO` request.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct QueryInfo {
    pub code: u32,
    pub start_id: u32,
    pub count: u32,
    /// Size of one item as the *driver* believes it to be. The device must
    /// refuse a mismatch rather than write a differently sized struct into a
    /// buffer the driver sized for something else.
    pub size: u32,
}

impl QueryInfo {
    pub fn parse(raw: &[u8; QUERY_INFO_LEN]) -> Self {
        Self {
            code: le32(raw, 0),
            start_id: le32(raw, 4),
            count: le32(raw, 8),
            size: le32(raw, 12),
        }
    }

    /// Round trip, for tests and fuzz corpora.
    pub fn encode(&self) -> [u8; QUERY_INFO_LEN] {
        let mut raw = [0u8; QUERY_INFO_LEN];
        put32(&mut raw, 0, self.code);
        put32(&mut raw, 4, self.start_id);
        put32(&mut raw, 8, self.count);
        put32(&mut raw, 12, self.size);
        raw
    }
}

// ------------------------------------------------------------- item headers

/// `struct virtio_snd_pcm_hdr` / `virtio_snd_jack_hdr` / `virtio_snd_chmap_hdr`
/// — all three are a code plus one identifier, so one parser serves them.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ItemHdr {
    pub code: u32,
    pub id: u32,
}

impl ItemHdr {
    pub fn parse(raw: &[u8; PCM_HDR_LEN]) -> Self {
        Self {
            code: le32(raw, 0),
            id: le32(raw, 4),
        }
    }

    pub fn encode(&self) -> [u8; PCM_HDR_LEN] {
        let mut raw = [0u8; PCM_HDR_LEN];
        put32(&mut raw, 0, self.code);
        put32(&mut raw, 4, self.id);
        raw
    }
}

// ------------------------------------------------------------- SET_PARAMS

/// `struct virtio_snd_pcm_set_params`, exactly as the guest wrote it. Every
/// field is untrusted; [`crate::stream::validate_params`] is the only thing
/// that turns one of these into something the host will act on.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct RawSetParams {
    pub stream_id: u32,
    pub buffer_bytes: u32,
    pub period_bytes: u32,
    pub features: u32,
    pub channels: u8,
    pub format: u8,
    pub rate: u8,
}

impl RawSetParams {
    pub fn parse(raw: &[u8; SET_PARAMS_LEN]) -> Self {
        Self {
            // raw[0..4] is the request code; the caller already dispatched on it.
            stream_id: le32(raw, 4),
            buffer_bytes: le32(raw, 8),
            period_bytes: le32(raw, 12),
            features: le32(raw, 16),
            channels: raw[20],
            format: raw[21],
            rate: raw[22],
            // raw[23] is padding.
        }
    }

    pub fn encode(&self) -> [u8; SET_PARAMS_LEN] {
        let mut raw = [0u8; SET_PARAMS_LEN];
        put32(&mut raw, 0, R_PCM_SET_PARAMS);
        put32(&mut raw, 4, self.stream_id);
        put32(&mut raw, 8, self.buffer_bytes);
        put32(&mut raw, 12, self.period_bytes);
        put32(&mut raw, 16, self.features);
        raw[20] = self.channels;
        raw[21] = self.format;
        raw[22] = self.rate;
        raw
    }
}

// --------------------------------------------------------------- info replies

/// `struct virtio_snd_jack_info`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct JackInfo {
    pub hda_fn_nid: u32,
    pub features: u32,
    pub hda_reg_defconf: u32,
    pub hda_reg_caps: u32,
    pub connected: bool,
}

impl JackInfo {
    pub fn encode(&self) -> [u8; JACK_INFO_LEN] {
        let mut raw = [0u8; JACK_INFO_LEN];
        put32(&mut raw, 0, self.hda_fn_nid);
        put32(&mut raw, 4, self.features);
        put32(&mut raw, 8, self.hda_reg_defconf);
        put32(&mut raw, 12, self.hda_reg_caps);
        raw[16] = u8::from(self.connected);
        raw
    }
}

/// `struct virtio_snd_pcm_info`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct PcmInfo {
    pub hda_fn_nid: u32,
    pub features: u32,
    /// `1 << VIRTIO_SND_PCM_FMT_*` bitmap.
    pub formats: u64,
    /// `1 << VIRTIO_SND_PCM_RATE_*` bitmap.
    pub rates: u64,
    pub direction: u8,
    pub channels_min: u8,
    pub channels_max: u8,
}

impl PcmInfo {
    pub fn encode(&self) -> [u8; PCM_INFO_LEN] {
        let mut raw = [0u8; PCM_INFO_LEN];
        put32(&mut raw, 0, self.hda_fn_nid);
        put32(&mut raw, 4, self.features);
        put64(&mut raw, 8, self.formats);
        put64(&mut raw, 16, self.rates);
        raw[24] = self.direction;
        raw[25] = self.channels_min;
        raw[26] = self.channels_max;
        raw
    }
}

/// `struct virtio_snd_chmap_info`.
#[derive(Debug, Clone, Copy)]
pub struct ChmapInfo {
    pub hda_fn_nid: u32,
    pub direction: u8,
    pub channels: u8,
    pub positions: [u8; CHMAP_MAX_SIZE],
}

impl ChmapInfo {
    /// A stereo output map (front left, front right).
    pub fn stereo_output() -> Self {
        Self::stereo(D_OUTPUT)
    }

    /// A stereo *input* map. Same two positions: a capture stream carries the
    /// same channels the playback one does, so the guest's mixer lines them up
    /// without a matrix.
    pub fn stereo_input() -> Self {
        Self::stereo(D_INPUT)
    }

    fn stereo(direction: u8) -> Self {
        let mut positions = [CHMAP_NONE; CHMAP_MAX_SIZE];
        positions[0] = CHMAP_FL;
        positions[1] = CHMAP_FR;
        Self {
            hda_fn_nid: 0,
            direction,
            channels: 2,
            positions,
        }
    }

    pub fn encode(&self) -> [u8; CHMAP_INFO_LEN] {
        let mut raw = [0u8; CHMAP_INFO_LEN];
        put32(&mut raw, 0, self.hda_fn_nid);
        raw[4] = self.direction;
        raw[5] = self.channels;
        // `channels` is at most CHMAP_MAX_SIZE by construction, and the copy is
        // bounded by the destination either way.
        let n = (self.channels as usize).min(CHMAP_MAX_SIZE);
        raw[6..6 + n].copy_from_slice(&self.positions[..n]);
        raw
    }
}

/// `struct virtio_snd_pcm_status` — the device-writable tail of every I/O
/// message.
pub fn encode_pcm_status(status: u32, latency_bytes: u32) -> [u8; PCM_STATUS_LEN] {
    let mut raw = [0u8; PCM_STATUS_LEN];
    put32(&mut raw, 0, status);
    put32(&mut raw, 4, latency_bytes);
    raw
}

/// `struct virtio_snd_event`.
pub fn encode_event(code: u32, data: u32) -> [u8; EVENT_LEN] {
    let mut raw = [0u8; EVENT_LEN];
    put32(&mut raw, 0, code);
    put32(&mut raw, 4, data);
    raw
}

// -------------------------------------------------- compile-time spec checks

// The C structs these mirror (linux/virtio_snd.h, SPDX BSD-3-Clause) are
// naturally aligned, so every length below is "sum of fields, padded to the
// widest member". Getting one wrong desynchronises the whole reply array, so
// they are asserted rather than trusted.
const _: () = {
    assert!(HDR_LEN == 4);
    assert!(EVENT_LEN == HDR_LEN + 4);
    assert!(QUERY_INFO_LEN == HDR_LEN + 12);
    assert!(JACK_HDR_LEN == HDR_LEN + 4);
    assert!(JACK_INFO_LEN == 4 + 4 + 4 + 4 + 1 + 7);
    assert!(JACK_REMAP_LEN == JACK_HDR_LEN + 8);
    assert!(PCM_HDR_LEN == HDR_LEN + 4);
    // hda_fn_nid, features, then two 8-byte bitmaps (8-aligned), then three
    // bytes of enums and five of padding.
    assert!(PCM_INFO_LEN == 4 + 4 + 8 + 8 + 1 + 1 + 1 + 5);
    assert!(SET_PARAMS_LEN == PCM_HDR_LEN + 4 + 4 + 4 + 1 + 1 + 1 + 1);
    assert!(PCM_XFER_LEN == 4);
    assert!(PCM_STATUS_LEN == 8);
    assert!(CHMAP_HDR_LEN == HDR_LEN + 4);
    assert!(CHMAP_INFO_LEN == 4 + 1 + 1 + CHMAP_MAX_SIZE);
    // The status codes are contiguous from S_OK, which the device relies on
    // nowhere but a reader might.
    assert!(S_BAD_MSG == S_OK + 1);
    assert!(S_NOT_SUPP == S_OK + 2);
    assert!(S_IO_ERR == S_OK + 3);
};

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_rate_table_matches_the_spec_enum() {
        assert_eq!(rate_hz(0), Some(5512));
        assert_eq!(rate_hz(RATE_44100), Some(44100));
        assert_eq!(rate_hz(RATE_48000), Some(48000));
        assert_eq!(rate_hz(RATE_192000), Some(192_000));
        assert_eq!(rate_hz(13), Some(384_000));
        assert_eq!(rate_hz(RATE_COUNT), None, "one past the enum is not a rate");
        assert_eq!(rate_hz(255), None);
    }

    #[test]
    fn set_params_round_trips_through_the_wire_format() {
        let params = RawSetParams {
            stream_id: 0,
            buffer_bytes: 8192,
            period_bytes: 2048,
            features: 0,
            channels: 2,
            format: FMT_S16,
            rate: RATE_48000,
        };
        let raw = params.encode();
        assert_eq!(request_code(&raw), R_PCM_SET_PARAMS);
        assert_eq!(RawSetParams::parse(&raw), params);
    }

    #[test]
    fn query_info_round_trips() {
        let query = QueryInfo {
            code: R_PCM_INFO,
            start_id: 0,
            count: 1,
            size: PCM_INFO_LEN as u32,
        };
        assert_eq!(QueryInfo::parse(&query.encode()), query);
    }

    /// The reply encoders must place every field where the driver's C struct
    /// expects it — an off-by-four here is a guest that reads garbage rates.
    #[test]
    fn pcm_info_lands_its_fields_at_the_c_offsets() {
        let info = PcmInfo {
            hda_fn_nid: 0x1111_1111,
            features: 0x2222_2222,
            formats: 0x3333_3333_4444_4444,
            rates: 0x5555_5555_6666_6666,
            direction: D_OUTPUT,
            channels_min: 1,
            channels_max: 2,
        };
        let raw = info.encode();
        assert_eq!(&raw[0..4], &0x1111_1111u32.to_le_bytes());
        assert_eq!(&raw[4..8], &0x2222_2222u32.to_le_bytes());
        assert_eq!(&raw[8..16], &0x3333_3333_4444_4444u64.to_le_bytes());
        assert_eq!(&raw[16..24], &0x5555_5555_6666_6666u64.to_le_bytes());
        assert_eq!(raw[24], D_OUTPUT);
        assert_eq!(raw[25], 1);
        assert_eq!(raw[26], 2);
        assert_eq!(&raw[27..32], &[0u8; 5], "padding is zero, not stack junk");
    }

    #[test]
    fn the_stereo_chmap_is_front_left_then_front_right() {
        for (map, direction) in [
            (ChmapInfo::stereo_output(), D_OUTPUT),
            (ChmapInfo::stereo_input(), D_INPUT),
        ] {
            let raw = map.encode();
            assert_eq!(raw[4], direction);
            assert_eq!(raw[5], 2);
            assert_eq!(raw[6], CHMAP_FL);
            assert_eq!(raw[7], CHMAP_FR);
            assert_eq!(&raw[8..], &[0u8; CHMAP_INFO_LEN - 8]);
        }
    }

    /// The two jack default-configuration dwords must name the two HDA device
    /// types a desktop shows as "Line Out" and "Microphone"; everything else
    /// in the field stays zero.
    #[test]
    fn the_jack_defconfs_name_a_line_out_and_a_microphone() {
        assert_eq!(DEFCONF_LINE_OUT, 0);
        assert_eq!(DEFCONF_MIC_IN, 0x00a0_0000);
        assert_eq!((DEFCONF_MIC_IN >> 20) & 0xf, 0xa);
        assert_eq!(DEFCONF_MIC_IN & !(0xf << 20), 0, "no other field is set");
    }

    #[test]
    fn a_short_buffer_reads_as_zero_rather_than_panicking() {
        // The device never calls these with a short slice, but the fuzz
        // targets do; a panic on a guest-controlled path is the bug.
        assert_eq!(le32(&[1, 2], 0), 0);
        assert_eq!(le32(&[1, 2, 3, 4], 4), 0);
        assert_eq!(request_code(&[]), 0);
    }
}
