//! The codec on link address 0: one audio function group with one output
//! converter feeding one fixed speaker pin.
//!
//! | NID | What | Answers |
//! |---|---|---|
//! | 0 | root | vendor and revision, one function group at NID 1 |
//! | 1 | audio function group | two widgets from NID 2, 16-bit 44.1/48 kHz PCM |
//! | 2 | audio output (DAC) | stereo, format and stream/channel verbs |
//! | 3 | pin complex | output-capable, fixed internal speaker, connected to NID 2 |
//!
//! That is the smallest graph Windows' in-box `HDAudio.sys` builds a
//! render endpoint from ("Speakers"), and Linux's `snd-hda-codec-generic`
//! from the same parameters. There is no input, no amplifier and no jack
//! detection: volume is the guest mixer's, applied before the samples reach
//! the stream.
//!
//! Every verb gets an answer; one this codec does not implement reads 0,
//! which is what an absent capability reads on real hardware.

/// Vendor and device of the codec (`0x1af4` is the virtio vendor, which a
/// generic class driver matches on nothing but the function group).
pub const VENDOR_DEVICE_ID: u32 = 0x1af4_0022;
/// Revision 1.0.
pub const REVISION_ID: u32 = 0x0010_0100;
/// What the subsystem-id verb answers.
pub const SUBSYSTEM_ID: u32 = 0x1af4_1100;

const NID_ROOT: u8 = 0;
const NID_AFG: u8 = 1;
/// The output converter, whose stream tag and format the controller reads.
pub const NID_DAC: u8 = 2;
const NID_PIN: u8 = 3;

/// `GetParameter` ids.
mod param {
    pub const VENDOR_ID: u8 = 0x00;
    pub const REVISION_ID: u8 = 0x02;
    pub const SUB_NODE_COUNT: u8 = 0x04;
    pub const FUNCTION_GROUP_TYPE: u8 = 0x05;
    pub const AFG_CAPS: u8 = 0x08;
    pub const WIDGET_CAPS: u8 = 0x09;
    pub const PCM: u8 = 0x0a;
    pub const STREAM_FORMATS: u8 = 0x0b;
    pub const PIN_CAPS: u8 = 0x0c;
    pub const CONN_LIST_LEN: u8 = 0x0e;
    pub const POWER_STATES: u8 = 0x0f;
}

/// 16-bit samples at 44.1 and 48 kHz.
const PCM_16_44_48: u32 = (1 << 17) | (1 << 5) | (1 << 6);
/// Audio output widget (type 0 in bits 23:20): stereo, format override.
const DAC_CAPS: u32 = (1 << 4) | 1;
/// Pin widget: connection list, stereo.
const PIN_CAPS_WIDGET: u32 = (4 << 20) | (1 << 8) | 1;
/// Pin capabilities: output capable.
const PIN_OUTPUT: u32 = 1 << 4;
/// Fixed function, internal, speaker, other analog, association 1.
const PIN_CONFIG_SPEAKER: u32 = 0x9017_0110;
/// D0–D3 supported.
const POWER_D0_D3: u32 = 0xf;

/// What the guest has set on the codec. Everything else is constant.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Codec {
    /// The DAC's converter format, in the stream-format encoding.
    pub dac_format: u16,
    /// The DAC's `ConverterStreamChannel`: stream tag in bits 7:4, lowest
    /// channel in 3:0.
    pub dac_stream: u8,
    pin_control: u8,
    pin_config: u32,
    eapd: u8,
    power: [u8; 4],
}

impl Default for Codec {
    fn default() -> Self {
        Self {
            dac_format: 0x0011, // 48 kHz, 16-bit, stereo
            dac_stream: 0,
            pin_control: 0,
            pin_config: PIN_CONFIG_SPEAKER,
            eapd: 0,
            power: [0; 4],
        }
    }
}

impl Codec {
    /// The stream tag the DAC listens to; 0 is "none".
    pub fn dac_stream_tag(&self) -> u8 {
        self.dac_stream >> 4
    }

    /// Runs one 32-bit command (codec address already matched) and returns
    /// its response.
    pub fn command(&mut self, verb: u32) -> u32 {
        let nid = ((verb >> 20) & 0x7f) as u8;
        // A 4-bit verb carries a 16-bit payload; the 12-bit verbs are the
        // ones whose top nibble is 7 (set) or F (get).
        let top = (verb >> 16) & 0xf;
        if top != 0x7 && top != 0xf {
            let payload = (verb & 0xffff) as u16;
            return match (top, nid) {
                (0x2, NID_DAC) => {
                    self.dac_format = payload;
                    0
                }
                (0xa, NID_DAC) => u32::from(self.dac_format),
                // Amplifier gain/mute: there are no amplifiers.
                _ => 0,
            };
        }
        let code = (verb >> 8) & 0xfff;
        let payload = (verb & 0xff) as u8;
        match code {
            0xf00 => self.parameter(nid, payload),
            0xf01 | 0x701 => 0,
            0xf02 if nid == NID_PIN => u32::from(NID_DAC),
            0xf05 => self
                .power
                .get(usize::from(nid))
                .map_or(0, |&state| u32::from(state) << 4 | u32::from(state)),
            0x705 => {
                if let Some(state) = self.power.get_mut(usize::from(nid)) {
                    *state = payload & 0x3;
                }
                0
            }
            0xf06 if nid == NID_DAC => u32::from(self.dac_stream),
            0x706 if nid == NID_DAC => {
                self.dac_stream = payload;
                0
            }
            0xf07 if nid == NID_PIN => u32::from(self.pin_control),
            0x707 if nid == NID_PIN => {
                self.pin_control = payload;
                0
            }
            // Pin sense: always present (a fixed speaker).
            0xf09 if nid == NID_PIN => 0x8000_0000,
            0xf0c if nid == NID_PIN => u32::from(self.eapd),
            0x70c if nid == NID_PIN => {
                self.eapd = payload;
                0
            }
            0xf1c if nid == NID_PIN => self.pin_config,
            0x71c..=0x71f if nid == NID_PIN => {
                let shift = (code - 0x71c) * 8;
                self.pin_config =
                    (self.pin_config & !(0xff << shift)) | (u32::from(payload) << shift);
                0
            }
            0xf20 if nid == NID_AFG => SUBSYSTEM_ID,
            // Function group reset.
            0x7ff if nid == NID_AFG => {
                *self = Self::default();
                0
            }
            _ => 0,
        }
    }

    fn parameter(&self, nid: u8, id: u8) -> u32 {
        match (nid, id) {
            (NID_ROOT, param::VENDOR_ID) => VENDOR_DEVICE_ID,
            (NID_ROOT, param::REVISION_ID) => REVISION_ID,
            (NID_ROOT, param::SUB_NODE_COUNT) => (u32::from(NID_AFG) << 16) | 1,
            (NID_AFG, param::SUB_NODE_COUNT) => (u32::from(NID_DAC) << 16) | 2,
            (NID_AFG, param::FUNCTION_GROUP_TYPE) => 0x01,
            (NID_AFG, param::AFG_CAPS) => 0,
            (NID_AFG | NID_DAC, param::PCM) => PCM_16_44_48,
            (NID_AFG | NID_DAC, param::STREAM_FORMATS) => 1,
            (NID_AFG, param::POWER_STATES) => POWER_D0_D3,
            (NID_DAC, param::WIDGET_CAPS) => DAC_CAPS,
            (NID_PIN, param::WIDGET_CAPS) => PIN_CAPS_WIDGET,
            (NID_PIN, param::PIN_CAPS) => PIN_OUTPUT,
            (NID_PIN, param::CONN_LIST_LEN) => 1,
            _ => 0,
        }
    }

    /// The guest-set state, for a snapshot.
    pub fn save(&self) -> [u8; 12] {
        let mut out = [0u8; 12];
        out[0..2].copy_from_slice(&self.dac_format.to_le_bytes());
        out[2] = self.dac_stream;
        out[3] = self.pin_control;
        out[4..8].copy_from_slice(&self.pin_config.to_le_bytes());
        out[8] = self.eapd;
        out[9] = self.power[NID_AFG as usize];
        out[10] = self.power[NID_DAC as usize];
        out[11] = self.power[NID_PIN as usize];
        out
    }

    /// Puts [`Self::save`] back.
    pub fn load(bytes: &[u8; 12]) -> Self {
        Self {
            dac_format: u16::from_le_bytes([bytes[0], bytes[1]]),
            dac_stream: bytes[2],
            pin_control: bytes[3],
            pin_config: u32::from_le_bytes([bytes[4], bytes[5], bytes[6], bytes[7]]),
            eapd: bytes[8],
            power: [0, bytes[9] & 3, bytes[10] & 3, bytes[11] & 3],
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn verb(nid: u8, code: u32, payload: u8) -> u32 {
        (u32::from(nid) << 20) | (code << 8) | u32::from(payload)
    }

    #[test]
    fn the_graph_is_root_afg_dac_pin() {
        let mut codec = Codec::default();
        assert_eq!(codec.command(verb(0, 0xf00, 0x00)), VENDOR_DEVICE_ID);
        assert_eq!(codec.command(verb(0, 0xf00, 0x04)), 0x0001_0001);
        assert_eq!(codec.command(verb(1, 0xf00, 0x05)) & 0xff, 1, "audio");
        assert_eq!(codec.command(verb(1, 0xf00, 0x04)), 0x0002_0002);
        assert_eq!(codec.command(verb(2, 0xf00, 0x09)) >> 20, 0, "output");
        assert_eq!(codec.command(verb(3, 0xf00, 0x09)) >> 20, 4, "pin");
        assert_eq!(codec.command(verb(3, 0xf02, 0)), 2, "pin <- DAC");
        assert_eq!(codec.command(verb(3, 0xf1c, 0)), PIN_CONFIG_SPEAKER);
    }

    #[test]
    fn the_dac_keeps_its_format_and_stream() {
        let mut codec = Codec::default();
        // 4-bit verb 0x2: set converter format 44.1 kHz 16-bit stereo.
        codec.command((2 << 20) | (0x2 << 16) | 0x4011);
        assert_eq!(codec.command((2 << 20) | (0xa << 16)), 0x4011);
        codec.command(verb(2, 0x706, 0x10));
        assert_eq!(codec.dac_stream_tag(), 1);
        let saved = codec.save();
        assert_eq!(Codec::load(&saved), codec);
        codec.command(verb(1, 0x7ff, 0));
        assert_eq!(codec, Codec::default());
    }

    #[test]
    fn unknown_verbs_and_nodes_read_zero() {
        let mut codec = Codec::default();
        assert_eq!(codec.command(verb(0x7f, 0xf00, 0)), 0);
        assert_eq!(codec.command(verb(2, 0xf99, 0)), 0);
        assert_eq!(codec.command(verb(0x7f, 0x705, 3)), 0);
    }
}
