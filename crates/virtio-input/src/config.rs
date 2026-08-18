//! The virtio-input configuration space (VirtIO spec 1.2, section 5.8.4,
//! `struct virtio_input_config`) — backlog MVP-901.
//!
//! This is the part every guest driver pokes at during probe, so it is worth
//! spelling the layout out:
//!
//! | Offset | Field | Access |
//! |---|---|---|
//! | 0 | `select` (u8) | driver-writable |
//! | 1 | `subsel` (u8) | driver-writable |
//! | 2 | `size` (u8) | read-only, length of the payload for the current pair |
//! | 3..8 | `reserved[5]` | read-only zeroes |
//! | 8..136 | payload union: string / bitmap / `absinfo` / `devids` | read-only |
//!
//! The driver writes `select` (and `subsel` where it matters), then reads
//! `size` and — if it is non-zero — that many payload bytes. A pair the device
//! does not implement reads back `size = 0`, which is how Linux's
//! `virtinput_cfg_select()` decides a capability is absent.
//!
//! Bitmaps are a flat little-endian byte stream where **bit N means code N**,
//! counted from the start of the payload: `KEY_A` (30) is byte 3 bit 6,
//! `BTN_LEFT` (0x110 = 272) is byte 34 bit 0. The payload caps the highest
//! representable code at 1023, which is above `KEY_MAX` (0x2ff).
//!
//! Nothing here touches guest memory or allocates per access: a read builds one
//! stack [`Selection`] and copies out of it.

use crate::{abs, btn, ev, key, rel, rep, ABS_AXIS_MAX};

// ------------------------------------------------------------------ layout

/// Offset of the `select` byte.
pub const SELECT: u64 = 0;
/// Offset of the `subsel` byte.
pub const SUBSEL: u64 = 1;
/// Offset of the `size` byte.
pub const SIZE: u64 = 2;
/// Offset of the payload union.
pub const PAYLOAD: u64 = 8;
/// Size of the payload union (`char string[128]` is the largest member).
pub const PAYLOAD_MAX: usize = 128;
/// Total size of the guest-visible config space.
pub const CONFIG_LEN: u64 = PAYLOAD + PAYLOAD_MAX as u64;

// --------------------------------------------------------------- selectors

/// `VIRTIO_INPUT_CFG_UNSET`: no selection, `size` reads 0.
pub const VIRTIO_INPUT_CFG_UNSET: u8 = 0x00;
/// `VIRTIO_INPUT_CFG_ID_NAME`: the device name string.
pub const VIRTIO_INPUT_CFG_ID_NAME: u8 = 0x01;
/// `VIRTIO_INPUT_CFG_ID_SERIAL`: serial number string. Not implemented —
/// VMHost input devices have no meaningful serial, so this reads `size = 0`.
///
/// Note for readers coming from the backlog: 0x02 is the *serial*, not the
/// device ids. `virtio_input.h` orders them `ID_NAME = 1, ID_SERIAL = 2,
/// ID_DEVIDS = 3`.
pub const VIRTIO_INPUT_CFG_ID_SERIAL: u8 = 0x02;
/// `VIRTIO_INPUT_CFG_ID_DEVIDS`: `struct virtio_input_devids`.
pub const VIRTIO_INPUT_CFG_ID_DEVIDS: u8 = 0x03;
/// `VIRTIO_INPUT_CFG_PROP_BITS`: `INPUT_PROP_*` bitmap. Not implemented; see
/// the TODO on [`Profile::AbsolutePointer`].
pub const VIRTIO_INPUT_CFG_PROP_BITS: u8 = 0x10;
/// `VIRTIO_INPUT_CFG_EV_BITS`: supported codes for the event type in `subsel`.
pub const VIRTIO_INPUT_CFG_EV_BITS: u8 = 0x11;
/// `VIRTIO_INPUT_CFG_ABS_INFO`: `struct virtio_input_absinfo` for the axis in
/// `subsel`.
pub const VIRTIO_INPUT_CFG_ABS_INFO: u8 = 0x12;

/// Size of `struct virtio_input_devids` (four little-endian u16s).
pub const DEVIDS_LEN: usize = 8;
/// Size of `struct virtio_input_absinfo` (five little-endian u32s).
pub const ABS_INFO_LEN: usize = 20;

/// `BUS_VIRTUAL` from `linux/input.h`: these devices are not on any real bus.
pub const BUS_VIRTUAL: u16 = 0x06;
/// Vendor id VMHost claims for its input devices. There is no registry for
/// `BUS_VIRTUAL` ids; this is "VM" in ASCII, matching
/// `virtio_core::mmio::VMHOST_VENDOR_ID`.
pub const VMHOST_INPUT_VENDOR: u16 = 0x564d;
/// Product id of the keyboard.
pub const PRODUCT_KEYBOARD: u16 = 0x0001;
/// Product id of the absolute pointer.
pub const PRODUCT_TABLET: u16 = 0x0002;
/// Version reported by both devices.
pub const INPUT_VERSION: u16 = 0x0001;

// ----------------------------------------------------------------- devids

/// `struct virtio_input_devids`, the evdev identity a driver exposes as
/// `/sys/class/input/input*/id/*`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct DevIds {
    pub bustype: u16,
    pub vendor: u16,
    pub product: u16,
    pub version: u16,
}

impl DevIds {
    /// Wire form: four little-endian u16s.
    pub fn to_le_bytes(self) -> [u8; DEVIDS_LEN] {
        let mut bytes = [0u8; DEVIDS_LEN];
        bytes[0..2].copy_from_slice(&self.bustype.to_le_bytes());
        bytes[2..4].copy_from_slice(&self.vendor.to_le_bytes());
        bytes[4..6].copy_from_slice(&self.product.to_le_bytes());
        bytes[6..8].copy_from_slice(&self.version.to_le_bytes());
        bytes
    }
}

// ---------------------------------------------------------------- profiles

/// Which device an [`crate::InputDevice`] is (MVP-901).
///
/// The profile decides the whole guest-visible personality: name, product id
/// and every `EV_BITS` / `ABS_INFO` answer. Nothing else about the device
/// differs, so the queue and event-delivery code is shared.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Profile {
    /// A keyboard: `EV_KEY` over the dense low key range plus `EV_REP`.
    Keyboard,

    /// A tablet-style absolute pointer: `ABS_X`/`ABS_Y` over the full
    /// [`ABS_AXIS_MAX`] range, the five mouse buttons and the scroll wheels.
    ///
    /// TODO(MVP-903 follow-up): consider advertising `INPUT_PROP_DIRECT` via
    /// `VIRTIO_INPUT_CFG_PROP_BITS`. It would tell libinput the surface is
    /// direct (like a touchscreen), which changes pointer-acceleration and
    /// calibration behaviour in the guest; QEMU's `virtio-tablet` does not set
    /// it either, so the MVP stays with plain absolute-pointer semantics until
    /// there is a Debian desktop to test the difference against.
    AbsolutePointer,
}

impl Profile {
    /// The `VIRTIO_INPUT_CFG_ID_NAME` string, as the guest shows it in
    /// `/proc/bus/input/devices`.
    pub const fn name(self) -> &'static str {
        match self {
            Profile::Keyboard => "VMHost Keyboard",
            Profile::AbsolutePointer => "VMHost Tablet",
        }
    }

    /// The evdev identity. Stable across runs so the guest's udev rules and
    /// desktop settings survive a VM restart.
    pub const fn devids(self) -> DevIds {
        DevIds {
            bustype: BUS_VIRTUAL,
            vendor: VMHOST_INPUT_VENDOR,
            product: match self {
                Profile::Keyboard => PRODUCT_KEYBOARD,
                Profile::AbsolutePointer => PRODUCT_TABLET,
            },
            version: INPUT_VERSION,
        }
    }

    /// The `EV_BITS` bitmap for one event type — the single source of truth for
    /// what this device can emit ([`Profile::accepts`] and
    /// [`crate::split_batch`] both consult it).
    ///
    /// The keyboard advertises key codes 1..=255 (`display`'s keymap stays
    /// inside that range, and covering it wholesale means a keymap addition
    /// cannot silently produce events the guest ignores) plus `KEY_SELECT`,
    /// the single code above it that the keymap can produce. `KEY_RESERVED`
    /// (0) is deliberately left out, and so is the `BTN_*` range at 0x100+ —
    /// that belongs to the pointer.
    pub fn event_bits(self, event_type: u16) -> Selection {
        match (self, event_type) {
            (Profile::Keyboard, ev::KEY) => Selection::bitmap(
                (key::RESERVED + 1..=key::DENSE_MAX).chain(std::iter::once(key::SELECT)),
            ),
            // Auto-repeat: the guest's input core generates the repeats, the
            // device only has to say it supports the two parameters.
            (Profile::Keyboard, ev::REP) => Selection::bitmap([rep::DELAY, rep::PERIOD]),
            (Profile::AbsolutePointer, ev::KEY) => {
                Selection::bitmap([btn::LEFT, btn::RIGHT, btn::MIDDLE, btn::SIDE, btn::EXTRA])
            }
            (Profile::AbsolutePointer, ev::ABS) => Selection::bitmap([abs::X, abs::Y]),
            // `display` emits both wheel axes, so both are advertised —
            // otherwise the guest silently discards horizontal scrolling.
            (Profile::AbsolutePointer, ev::REL) => Selection::bitmap([rel::HWHEEL, rel::WHEEL]),
            // EV_SYN is implicit (Linux never queries it), and EV_MSC/EV_SW/
            // EV_LED/EV_SND are not produced by either device.
            _ => Selection::EMPTY,
        }
    }

    /// The `ABS_INFO` answer for one axis: the full window range, no filtering.
    ///
    /// `fuzz`/`flat` are 0 because the host coordinates are already exact — a
    /// non-zero deadzone would make the guest cursor lag the host one — and
    /// `res` is 0 because there is no physical unit behind a window pixel.
    pub fn abs_info(self, axis: u16) -> Selection {
        match (self, axis) {
            (Profile::AbsolutePointer, abs::X | abs::Y) => {
                Selection::from_slice(&abs_info_bytes(0, ABS_AXIS_MAX, 0, 0, 0))
            }
            _ => Selection::EMPTY,
        }
    }

    /// Whether this device advertises `event`'s `(type, code)` pair.
    /// `SYN_REPORT` belongs to every device.
    pub fn accepts(self, event: crate::InputEvent) -> bool {
        if event.event_type == ev::SYN {
            return event.code == 0;
        }
        self.event_bits(event.event_type).contains(event.code)
    }
}

/// `struct virtio_input_absinfo` on the wire.
fn abs_info_bytes(min: u32, max: u32, fuzz: u32, flat: u32, res: u32) -> [u8; ABS_INFO_LEN] {
    let mut bytes = [0u8; ABS_INFO_LEN];
    for (slot, value) in bytes.chunks_exact_mut(4).zip([min, max, fuzz, flat, res]) {
        slot.copy_from_slice(&value.to_le_bytes());
    }
    bytes
}

// --------------------------------------------------------------- selection

/// The payload the device exposes for one `(select, subsel)` pair.
///
/// A fixed-size buffer rather than a `Vec` so serving a config read never
/// allocates, and so `size` provably fits the u8 register.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Selection {
    len: usize,
    bytes: [u8; PAYLOAD_MAX],
}

impl Selection {
    /// The answer for every pair the device does not implement: `size = 0`.
    pub const EMPTY: Selection = Selection {
        len: 0,
        bytes: [0u8; PAYLOAD_MAX],
    };

    /// Copies `src` into the payload, truncating at [`PAYLOAD_MAX`].
    pub fn from_slice(src: &[u8]) -> Self {
        let len = src.len().min(PAYLOAD_MAX);
        let mut bytes = [0u8; PAYLOAD_MAX];
        bytes[..len].copy_from_slice(&src[..len]);
        Self { len, bytes }
    }

    /// Builds a bitmap where bit N is set for every code N in `codes`. The
    /// length is trimmed to the last byte that carries a set bit, which is what
    /// the driver uses to size its own bitmap. Codes that do not fit the
    /// payload are dropped rather than wrapping around.
    pub fn bitmap(codes: impl IntoIterator<Item = u16>) -> Self {
        let mut bytes = [0u8; PAYLOAD_MAX];
        let mut len = 0usize;
        for code in codes {
            let index = usize::from(code / 8);
            if index >= PAYLOAD_MAX {
                continue;
            }
            bytes[index] |= 1u8 << (code % 8);
            len = len.max(index + 1);
        }
        Self { len, bytes }
    }

    /// Value of the `size` register: the payload length. Always at most
    /// [`PAYLOAD_MAX`], so it fits the u8 field.
    pub fn size(&self) -> u8 {
        u8::try_from(self.len).unwrap_or(0)
    }

    /// The payload bytes the guest can read at offset [`PAYLOAD`].
    pub fn payload(&self) -> &[u8] {
        &self.bytes[..self.len]
    }

    /// True when this pair is not implemented (`size = 0`).
    pub fn is_empty(&self) -> bool {
        self.len == 0
    }

    /// Bit test, for interpreting a [`Selection`] built by [`bitmap`](Self::bitmap).
    pub fn contains(&self, code: u16) -> bool {
        let index = usize::from(code / 8);
        index < self.len && self.bytes[index] & (1u8 << (code % 8)) != 0
    }
}

/// Resolves one `(select, subsel)` pair against a profile.
///
/// Everything the device does not implement — including reserved and unknown
/// selectors — answers [`Selection::EMPTY`], never an error: `select` is
/// guest-controlled and probing unknown selectors is normal driver behaviour.
pub fn selection(profile: Profile, select: u8, subsel: u8) -> Selection {
    match select {
        VIRTIO_INPUT_CFG_ID_NAME => Selection::from_slice(profile.name().as_bytes()),
        VIRTIO_INPUT_CFG_ID_DEVIDS => Selection::from_slice(&profile.devids().to_le_bytes()),
        VIRTIO_INPUT_CFG_EV_BITS => profile.event_bits(u16::from(subsel)),
        VIRTIO_INPUT_CFG_ABS_INFO => profile.abs_info(u16::from(subsel)),
        _ => Selection::EMPTY,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::InputEvent;

    /// Bit position of a code inside a bitmap payload, spelled out the way the
    /// spec does: byte `code / 8`, bit `code % 8`.
    fn bit(payload: &[u8], code: u16) -> bool {
        let index = usize::from(code / 8);
        payload
            .get(index)
            .is_some_and(|byte| byte & (1u8 << (code % 8)) != 0)
    }

    #[test]
    fn layout_offsets_match_the_struct() {
        assert_eq!((SELECT, SUBSEL, SIZE, PAYLOAD), (0, 1, 2, 8));
        assert_eq!(CONFIG_LEN, 136);
        assert_eq!(PAYLOAD_MAX, 128);
        assert_eq!(DEVIDS_LEN, 8);
        assert_eq!(ABS_INFO_LEN, 20);
    }

    #[test]
    fn selector_values_match_virtio_input_h() {
        assert_eq!(VIRTIO_INPUT_CFG_UNSET, 0x00);
        assert_eq!(VIRTIO_INPUT_CFG_ID_NAME, 0x01);
        assert_eq!(VIRTIO_INPUT_CFG_ID_SERIAL, 0x02);
        assert_eq!(VIRTIO_INPUT_CFG_ID_DEVIDS, 0x03);
        assert_eq!(VIRTIO_INPUT_CFG_PROP_BITS, 0x10);
        assert_eq!(VIRTIO_INPUT_CFG_EV_BITS, 0x11);
        assert_eq!(VIRTIO_INPUT_CFG_ABS_INFO, 0x12);
    }

    // ------------------------------------------------------------- names

    #[test]
    fn name_selector_returns_the_device_name_without_a_nul() {
        let keyboard = selection(Profile::Keyboard, VIRTIO_INPUT_CFG_ID_NAME, 0);
        assert_eq!(keyboard.payload(), b"VMHost Keyboard");
        assert_eq!(keyboard.size(), 15);

        let tablet = selection(Profile::AbsolutePointer, VIRTIO_INPUT_CFG_ID_NAME, 0);
        assert_eq!(tablet.payload(), b"VMHost Tablet");
        assert_eq!(tablet.size(), 13);

        // subsel is irrelevant for the name and must not change the answer.
        for subsel in [0u8, 1, 0x11, 0xff] {
            assert_eq!(
                selection(Profile::Keyboard, VIRTIO_INPUT_CFG_ID_NAME, subsel).payload(),
                b"VMHost Keyboard"
            );
        }
    }

    // ------------------------------------------------------------ devids

    #[test]
    fn devids_selector_returns_four_little_endian_u16s() {
        let keyboard = selection(Profile::Keyboard, VIRTIO_INPUT_CFG_ID_DEVIDS, 0);
        assert_eq!(keyboard.size(), 8);
        assert_eq!(
            keyboard.payload(),
            &[0x06, 0x00, 0x4d, 0x56, 0x01, 0x00, 0x01, 0x00]
        );

        let tablet = selection(Profile::AbsolutePointer, VIRTIO_INPUT_CFG_ID_DEVIDS, 0);
        assert_eq!(
            tablet.payload(),
            &[0x06, 0x00, 0x4d, 0x56, 0x02, 0x00, 0x01, 0x00]
        );

        assert_eq!(
            Profile::Keyboard.devids(),
            DevIds {
                bustype: BUS_VIRTUAL,
                vendor: VMHOST_INPUT_VENDOR,
                product: PRODUCT_KEYBOARD,
                version: INPUT_VERSION,
            }
        );
        assert_ne!(
            Profile::Keyboard.devids().product,
            Profile::AbsolutePointer.devids().product,
            "the two devices must be distinguishable"
        );
    }

    // -------------------------------------------------------- keyboard bits

    #[test]
    fn keyboard_key_bitmap_covers_the_dense_range_and_key_select() {
        let bits = selection(
            Profile::Keyboard,
            VIRTIO_INPUT_CFG_EV_BITS,
            ev::KEY as u8, // EV_KEY = 1
        );
        // KEY_SELECT (353) is the highest advertised code: byte 44 bit 1.
        assert_eq!(bits.size(), 45);
        let payload = bits.payload();

        // Byte 0 covers codes 0..=7 with KEY_RESERVED (0) left clear.
        assert_eq!(payload[0], 0xfe);
        // Codes 8..=255 are all advertised.
        assert_eq!(&payload[1..32], &[0xffu8; 31]);
        // The BTN_* range (0x100..) belongs to the pointer, not here.
        assert_eq!(&payload[32..44], &[0u8; 12]);
        assert_eq!(payload[44], 0x02);

        // Spot checks against `linux/input-event-codes.h`, computed the long
        // way round: KEY_A = 30 => byte 3, bit 6.
        assert!(bit(payload, 30));
        assert_eq!(payload[3] & (1 << 6), 1 << 6);
        for code in [1u16, 28, 30, 42, 56, 57, 59, 88, 103, 111, 255, key::SELECT] {
            assert!(bit(payload, code), "KEY code {code} must be advertised");
        }
        for code in [
            key::RESERVED,
            256,
            271,
            btn::LEFT,
            btn::EXTRA,
            352,
            354,
            700,
        ] {
            assert!(!bit(payload, code), "code {code} must not be advertised");
        }
    }

    #[test]
    fn keyboard_advertises_auto_repeat() {
        let bits = selection(Profile::Keyboard, VIRTIO_INPUT_CFG_EV_BITS, ev::REP as u8);
        assert_eq!(bits.size(), 1, "a non-zero size is what enables EV_REP");
        assert_eq!(bits.payload(), &[0x03], "REP_DELAY and REP_PERIOD");
    }

    #[test]
    fn keyboard_has_no_pointer_capabilities() {
        for subsel in [ev::ABS, ev::REL, ev::MSC, ev::SW, ev::LED, ev::SND] {
            assert!(
                selection(Profile::Keyboard, VIRTIO_INPUT_CFG_EV_BITS, subsel as u8).is_empty(),
                "keyboard must not advertise event type {subsel}"
            );
        }
        for subsel in [abs::X as u8, abs::Y as u8, 0x40, 0xff] {
            assert!(selection(Profile::Keyboard, VIRTIO_INPUT_CFG_ABS_INFO, subsel).is_empty());
        }
    }

    // --------------------------------------------------------- pointer bits

    #[test]
    fn pointer_key_bitmap_carries_exactly_the_five_buttons() {
        let bits = selection(
            Profile::AbsolutePointer,
            VIRTIO_INPUT_CFG_EV_BITS,
            ev::KEY as u8,
        );
        // BTN_LEFT = 0x110 = 272 => byte 34, bit 0; BTN_EXTRA = 0x114 => bit 4.
        assert_eq!(bits.size(), 35);
        let payload = bits.payload();
        assert_eq!(&payload[..34], &[0u8; 34]);
        assert_eq!(payload[34], 0x1f);
        assert!(bit(payload, btn::LEFT));
        assert_eq!(payload[34] & 1, 1);
        for code in [btn::LEFT, btn::RIGHT, btn::MIDDLE, btn::SIDE, btn::EXTRA] {
            assert!(bit(payload, code), "BTN code {code:#x} must be advertised");
        }
        for code in [30u16, 0x115, 0x116, 0x10f] {
            assert!(!bit(payload, code), "code {code:#x} must not be advertised");
        }
    }

    #[test]
    fn pointer_abs_bitmap_carries_x_and_y() {
        let bits = selection(
            Profile::AbsolutePointer,
            VIRTIO_INPUT_CFG_EV_BITS,
            ev::ABS as u8,
        );
        assert_eq!(bits.size(), 1);
        assert_eq!(bits.payload(), &[0x03]);
    }

    #[test]
    fn pointer_rel_bitmap_carries_both_wheels() {
        let bits = selection(
            Profile::AbsolutePointer,
            VIRTIO_INPUT_CFG_EV_BITS,
            ev::REL as u8,
        );
        // REL_HWHEEL = 6 => byte 0 bit 6; REL_WHEEL = 8 => byte 1 bit 0.
        assert_eq!(bits.size(), 2);
        assert_eq!(bits.payload(), &[0x40, 0x01]);
        assert!(bit(bits.payload(), rel::HWHEEL));
        assert!(bit(bits.payload(), rel::WHEEL));
        // No REL_X/REL_Y: this is an absolute device.
        assert!(!bit(bits.payload(), 0));
        assert!(!bit(bits.payload(), 1));
    }

    #[test]
    fn pointer_abs_info_spans_the_whole_axis_range() {
        for axis in [abs::X, abs::Y] {
            let info = selection(
                Profile::AbsolutePointer,
                VIRTIO_INPUT_CFG_ABS_INFO,
                axis as u8,
            );
            assert_eq!(info.size(), 20);
            assert_eq!(
                info.payload(),
                &[
                    0x00, 0x00, 0x00, 0x00, // min = 0
                    0xff, 0x7f, 0x00, 0x00, // max = 32767
                    0x00, 0x00, 0x00, 0x00, // fuzz = 0
                    0x00, 0x00, 0x00, 0x00, // flat = 0
                    0x00, 0x00, 0x00, 0x00, // res = 0
                ]
            );
        }
        // Any other axis is not implemented.
        for axis in [0x02u8, 0x03, 0x08, 0x28, 0xff] {
            assert!(
                selection(Profile::AbsolutePointer, VIRTIO_INPUT_CFG_ABS_INFO, axis).is_empty()
            );
        }
    }

    #[test]
    fn pointer_does_not_advertise_keys_or_repeat() {
        for subsel in [ev::REP, ev::MSC, ev::LED, ev::SND, ev::SW] {
            assert!(selection(
                Profile::AbsolutePointer,
                VIRTIO_INPUT_CFG_EV_BITS,
                subsel as u8
            )
            .is_empty());
        }
    }

    // ------------------------------------------------- unsupported selectors

    #[test]
    fn unsupported_and_unknown_selectors_report_size_zero() {
        for profile in [Profile::Keyboard, Profile::AbsolutePointer] {
            for select in [
                VIRTIO_INPUT_CFG_UNSET,
                VIRTIO_INPUT_CFG_ID_SERIAL,
                VIRTIO_INPUT_CFG_PROP_BITS,
                0x04,
                0x13,
                0x7f,
                0xff,
            ] {
                for subsel in [0u8, 1, 0x11, 0xff] {
                    let answer = selection(profile, select, subsel);
                    assert!(
                        answer.is_empty(),
                        "{profile:?} select {select:#x}/{subsel:#x} must be empty"
                    );
                    assert_eq!(answer.size(), 0);
                    assert_eq!(answer.payload(), &[] as &[u8]);
                }
            }
        }
    }

    #[test]
    fn every_ev_bits_subsel_is_answered_without_panicking() {
        // The guest can write any of the 256 subsel values for any of the 256
        // selectors; none of them may panic and every size must fit the
        // payload.
        for profile in [Profile::Keyboard, Profile::AbsolutePointer] {
            for select in 0..=u8::MAX {
                for subsel in 0..=u8::MAX {
                    let answer = selection(profile, select, subsel);
                    assert!(usize::from(answer.size()) <= PAYLOAD_MAX);
                    assert_eq!(answer.payload().len(), usize::from(answer.size()));
                }
            }
        }
    }

    // ---------------------------------------------------------- selection api

    #[test]
    fn bitmap_drops_codes_beyond_the_payload() {
        // 1024 needs byte 128, one past the union.
        let bits = Selection::bitmap([1023u16, 1024, 2000, u16::MAX]);
        assert_eq!(bits.size(), 128);
        assert!(bits.contains(1023));
        assert!(!bits.contains(1024));
        assert!(!bits.contains(u16::MAX));
    }

    #[test]
    fn from_slice_truncates_at_the_payload_size() {
        let long = vec![0xabu8; PAYLOAD_MAX + 64];
        let selection = Selection::from_slice(&long);
        assert_eq!(selection.size(), 128);
        assert_eq!(selection.payload().len(), PAYLOAD_MAX);
    }

    #[test]
    fn empty_selection_contains_nothing() {
        assert!(Selection::EMPTY.is_empty());
        for code in [0u16, 1, 30, 272, 1023, u16::MAX] {
            assert!(!Selection::EMPTY.contains(code));
        }
    }

    // ------------------------------------------------------------- accepts

    #[test]
    fn accepts_follows_the_advertised_bitmaps() {
        let key = |code| InputEvent {
            event_type: ev::KEY,
            code,
            value: 1,
        };
        assert!(Profile::Keyboard.accepts(key(30)));
        assert!(Profile::Keyboard.accepts(key(key::SELECT)));
        assert!(!Profile::Keyboard.accepts(key(key::RESERVED)));
        assert!(!Profile::Keyboard.accepts(key(btn::LEFT)));
        assert!(Profile::AbsolutePointer.accepts(key(btn::MIDDLE)));
        assert!(!Profile::AbsolutePointer.accepts(key(30)));

        let abs_x = InputEvent::abs_from_window(abs::X, 1.0, 2.0);
        assert!(Profile::AbsolutePointer.accepts(abs_x));
        assert!(!Profile::Keyboard.accepts(abs_x));

        // SYN_REPORT belongs to both; a bogus EV_SYN code does not.
        assert!(Profile::Keyboard.accepts(InputEvent::SYN_REPORT));
        assert!(Profile::AbsolutePointer.accepts(InputEvent::SYN_REPORT));
        assert!(!Profile::Keyboard.accepts(InputEvent {
            event_type: ev::SYN,
            code: 7,
            value: 0
        }));
    }
}
