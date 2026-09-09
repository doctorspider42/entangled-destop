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

use crate::{abs, btn, ev, key, msc, pad, rel, rep, ABS_AXIS_MAX};

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
/// `VIRTIO_INPUT_CFG_ID_SERIAL`: serial number string.
///
/// The keyboard and the tablet have nothing to say here and answer `size = 0`.
/// A **gamepad** answers [`player_serial`], because a two-player VM has two
/// devices with the same name and the same `input_id` — exactly as two
/// identical controllers on a real machine do — and the serial is where the
/// kernel puts the thing that tells them apart: `virtio_input.c` assigns it to
/// `idev->uniq`, which surfaces as `U: Uniq=` in `/proc/bus/input/devices` and
/// as `SDL_GetJoystickSerial`.
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
/// Vendor id Entangled Desktop claims for its input devices. There is no registry for
/// `BUS_VIRTUAL` ids; this is "VM" in ASCII, matching
/// `virtio_core::mmio::VMHOST_VENDOR_ID`.
pub const VMHOST_INPUT_VENDOR: u16 = 0x564d;
/// Product id of the keyboard.
pub const PRODUCT_KEYBOARD: u16 = 0x0001;
/// Product id of the absolute pointer.
pub const PRODUCT_TABLET: u16 = 0x0002;
/// Product id of the gamepad (GAME-2104).
pub const PRODUCT_GAMEPAD: u16 = 0x0003;
/// Version reported by every device.
pub const INPUT_VERSION: u16 = 0x0001;

/// The serial a gamepad publishes for `player` (0-based), one-based in the
/// text because "player 0" is not a thing anyone says out loud.
pub fn player_serial(player: usize) -> String {
    format!("player-{}", player.saturating_add(1))
}

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
    /// [`ABS_AXIS_MAX`] range, the three primary mouse buttons and the scroll
    /// wheels.
    ///
    /// # Why exactly three buttons, and why `EV_MSC`
    ///
    /// Until GAME-2104's follow-up this profile also advertised `BTN_SIDE` and
    /// `BTN_EXTRA` and no `EV_MSC`, and the cost of that was **`joydev` binding
    /// the tablet**: a VM with both a tablet and a gamepad had the *tablet* on
    /// `/dev/input/js0` and the pad on `js1`, so anything that opens "the first
    /// joystick" by number found a two-axis pointer.
    ///
    /// `joydev`'s id table matches any `EV_ABS` device with `ABS_X`, which a
    /// tablet is; the only way out is `joydev_dev_is_absolute_mouse()`, whose
    /// rule (drivers/input/joydev.c, unchanged from v6.12 to master) is three
    /// *exact* bitmap comparisons:
    ///
    /// 1. the event types are exactly `{SYN, KEY, ABS}`, or `{SYN, KEY, ABS,
    ///    MSC}`, or `{SYN, KEY, ABS, MSC, REL}` — note that `EV_REL` is
    ///    admissible only **together with** `EV_MSC`;
    /// 2. the absolute axes are exactly `{ABS_X, ABS_Y}`;
    /// 3. the keys are exactly `{BTN_LEFT, BTN_RIGHT, BTN_MIDDLE}`.
    ///
    /// A scroll wheel means `EV_REL`, so this profile has to reach rule 1's
    /// third form: hence the otherwise-pointless `EV_MSC`/`MSC_SCAN`, which a
    /// real USB mouse carries anyway (the HID core adds it) and which the
    /// kernel comment names the QEMU USB tablet for. And rule 3 is why the two
    /// extra buttons had to go; the host's Back and Forward now arrive as
    /// `KEY_BACK`/`KEY_FORWARD` on the *keyboard* device, which is what a
    /// multimedia keyboard sends and what browsers already bind — so nothing
    /// was lost, it moved.
    ///
    /// [`joydev_sees_an_absolute_mouse`] models the rule against this crate's
    /// own bitmaps, and there is a test; change any of the three arms above and
    /// it will tell you which one you broke.
    ///
    /// Do **not** reach for `BTN_TOUCH` or `BTN_DIGI` as a shortcut: they would
    /// also keep joydev away, and would additionally lie to libinput about what
    /// kind of surface this is.
    ///
    /// TODO(MVP-903 follow-up): consider advertising `INPUT_PROP_DIRECT` via
    /// `VIRTIO_INPUT_CFG_PROP_BITS`. It would tell libinput the surface is
    /// direct (like a touchscreen), which changes pointer-acceleration and
    /// calibration behaviour in the guest; QEMU's `virtio-tablet` does not set
    /// it either, so the MVP stays with plain absolute-pointer semantics until
    /// there is a Debian desktop to test the difference against.
    AbsolutePointer,

    /// An Xbox-shaped gamepad (GAME-2104): eleven buttons, two sticks, two
    /// analogue triggers and a hat D-pad.
    ///
    /// # Why this shape
    ///
    /// Nothing in the guest reads the device *name* to decide how to drive a
    /// pad; every consumer reads the capability bitmaps and the `ABS_INFO`
    /// ranges, and each of them has a different idea of what a "normal" pad
    /// looks like. The one layout all of them agree on is the one the kernel's
    /// own `xpad` driver publishes, so that is the one this profile copies
    /// code-for-code and range-for-range:
    ///
    /// * **The input core / `joydev`.** `joydev_match()` binds anything with
    ///   `EV_ABS`/`ABS_X` that is not a touchscreen, a digitiser or an
    ///   absolute mouse — so the pad turns up as `/dev/input/js*` as well as
    ///   `/dev/input/event*`. (The *tablet* profile is deliberately shaped to
    ///   fail that same match — see [`Profile::AbsolutePointer`] — which is
    ///   what leaves `js0` to the first pad.)
    /// * **udev.** `input_id` tags a device `ID_INPUT_JOYSTICK` when it
    ///   carries a key in the `BTN_JOYSTICK`..`BTN_DIGI` block; `BTN_SOUTH`
    ///   is in it. That tag is what gives the logged-in user an ACL on the
    ///   device node, so without it the pad exists and nothing can open it.
    /// * **SDL, and therefore Steam and most games.** SDL looks its
    ///   controller database up by a GUID built from bus/vendor/product, and
    ///   ours will never be in that database. What saves it is SDL's evdev
    ///   fallback (`LINUX_JoystickGetGamepadMapping` in SDL2,
    ///   `SDL_CreateGamepadMappingFromCapabilities` in SDL3): given
    ///   `BTN_SOUTH`/`EAST`/`NORTH`/`WEST`, `BTN_TL`/`TR`,
    ///   `BTN_SELECT`/`START`/`MODE`, `BTN_THUMBL`/`THUMBR`, `ABS_X`/`Y`,
    ///   `ABS_RX`/`RY`, `ABS_Z`/`RZ` and `ABS_HAT0X`/`Y` it *derives* a
    ///   complete gamepad mapping. Advertise exactly that set and the pad is
    ///   auto-mapped; advertise a sixth face button, or `BTN_TRIGGER`, or
    ///   digital `BTN_DPAD_*` in place of the hat, and the fallback either
    ///   guesses a different device class or declines.
    ///
    /// So: **an Xbox 360-shaped pad, with honest ids.** The one thing
    /// deliberately *not* copied from `xpad` is its USB vendor/product pair —
    /// claiming `045e:028e` would buy a built-in SDL database entry by
    /// impersonating Microsoft hardware, and the capability fallback above
    /// makes it unnecessary. The bus stays [`BUS_VIRTUAL`], which is what this
    /// device actually is.
    ///
    /// Not advertised, each for a reason: `EV_FF` (Linux's `virtio_input.c`
    /// never queries it and the spec has no channel to upload an effect
    /// through, so the bit would be a claim nothing can act on — the whole
    /// account is on [`crate::gamepad`]), `BTN_C`/`BTN_Z` (that is the
    /// six-face-button layout), `BTN_TRIGGER_HAPPY*` (`xpad`'s
    /// `dpad_to_buttons` mode, which is off by default) and `INPUT_PROP_*`
    /// (`xpad` sets none either).
    Gamepad,
}

impl Profile {
    /// Every profile there is. Exhaustive tests iterate this, so a profile
    /// added without a matching arm somewhere fails a test rather than
    /// quietly answering `size = 0` to everything.
    pub const ALL: [Profile; 3] = [
        Profile::Keyboard,
        Profile::AbsolutePointer,
        Profile::Gamepad,
    ];

    /// The `VIRTIO_INPUT_CFG_ID_NAME` string, as the guest shows it in
    /// `/proc/bus/input/devices`.
    pub const fn name(self) -> &'static str {
        match self {
            Profile::Keyboard => "Entangled Keyboard",
            Profile::AbsolutePointer => "Entangled Tablet",
            Profile::Gamepad => "Entangled Gamepad",
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
                Profile::Gamepad => PRODUCT_GAMEPAD,
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
            // Exactly the three primary buttons, and nothing else: see
            // [`Profile::AbsolutePointer`] and [`joydev_sees_an_absolute_mouse`].
            (Profile::AbsolutePointer, ev::KEY) => {
                Selection::bitmap([btn::LEFT, btn::RIGHT, btn::MIDDLE])
            }
            // Advertised for one reason only, and never sent: joydev's
            // absolute-mouse rule accepts `EV_REL` only in company with
            // `EV_MSC`, so a pointer with a scroll wheel needs both or neither.
            (Profile::AbsolutePointer, ev::MSC) => Selection::bitmap([msc::SCAN]),
            (Profile::AbsolutePointer, ev::ABS) => Selection::bitmap([abs::X, abs::Y]),
            // `display` emits both wheel axes, so both are advertised —
            // otherwise the guest silently discards horizontal scrolling.
            (Profile::AbsolutePointer, ev::REL) => Selection::bitmap([rel::HWHEEL, rel::WHEEL]),
            (Profile::Gamepad, ev::KEY) => Selection::bitmap(btn::GAMEPAD),
            (Profile::Gamepad, ev::ABS) => Selection::bitmap([
                abs::X,
                abs::Y,
                abs::Z,
                abs::RX,
                abs::RY,
                abs::RZ,
                abs::HAT0X,
                abs::HAT0Y,
            ]),
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
                Selection::from_slice(&abs_info_bytes(0, ABS_AXIS_MAX as i32, 0, 0, 0))
            }
            // Sticks, triggers and hat, with `xpad`'s own numbers (see
            // [`Profile::Gamepad`] and [`crate::pad`]).
            (Profile::Gamepad, abs::X | abs::Y | abs::RX | abs::RY) => {
                Selection::from_slice(&abs_info_bytes(
                    pad::STICK_MIN,
                    pad::STICK_MAX,
                    pad::STICK_FUZZ,
                    pad::STICK_FLAT,
                    0,
                ))
            }
            (Profile::Gamepad, abs::Z | abs::RZ) => {
                Selection::from_slice(&abs_info_bytes(pad::TRIGGER_MIN, pad::TRIGGER_MAX, 0, 0, 0))
            }
            // A hat has three states; fuzz or flat on it would quantise a
            // value that is already quantised.
            (Profile::Gamepad, abs::HAT0X | abs::HAT0Y) => {
                Selection::from_slice(&abs_info_bytes(pad::HAT_MIN, pad::HAT_MAX, 0, 0, 0))
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

// ------------------------------------------------------------ joydev model
//
// A pure, portable model of how the guest kernel's `joydev` handler classifies
// each profile. It exists because the classification is decided *here*, by the
// bitmaps this module publishes, and gets its verdict a whole guest boot away —
// so without a model, a one-bit change to a capability set is only ever caught
// by a KVM-only acceptance test on a machine that has the bootstrap kernel.
//
// Everything below mirrors drivers/input/joydev.c as of v6.12 (and unchanged in
// master). The one thing it cannot model is what the *host* does not decide:
// which minor a bound device gets, which is registration order.

/// `BTN_JOYSTICK`, the base of the joystick key block joydev's id table
/// matches on.
pub const BTN_JOYSTICK: u16 = 0x120;
/// `BTN_GAMEPAD` — the same code as [`btn::SOUTH`], named as joydev names it.
pub const BTN_GAMEPAD: u16 = 0x130;
/// `BTN_TRIGGER_HAPPY`.
pub const BTN_TRIGGER_HAPPY: u16 = 0x2c0;
/// `ABS_WHEEL`, one of the four axes joydev's id table matches on.
pub const ABS_WHEEL: u16 = 0x08;
/// `ABS_THROTTLE`.
pub const ABS_THROTTLE: u16 = 0x06;

/// The `EV_*` types the guest driver will set for `profile`, in ascending
/// order.
///
/// This is what Linux's `virtio_input.c` builds: `EV_SYN` is implicit for every
/// input device, and every other type is set exactly when its `EV_BITS`
/// selector answers a non-empty payload (`virtinput_cfg_bits()` returns early
/// on `size == 0`, so an empty bitmap sets no `evbit`).
pub fn advertised_event_types(profile: Profile) -> Vec<u16> {
    let mut types = vec![ev::SYN];
    types.extend((0..ev::CNT).filter(|&t| t != ev::SYN && !profile.event_bits(t).is_empty()));
    types
}

/// The codes `profile` advertises for one event type, in ascending order.
fn advertised_codes(profile: Profile, event_type: u16, max: u16) -> Vec<u16> {
    let bits = profile.event_bits(event_type);
    (0..max).filter(|&code| bits.contains(code)).collect()
}

/// Whether `joydev_dev_is_absolute_mouse()` would call this profile an
/// absolute mouse — the *only* way an `EV_ABS`/`ABS_X` device escapes joydev.
///
/// The three exact bitmap comparisons, in the kernel's own order. Note the
/// third accepted event-type set: `EV_REL` is admissible only alongside
/// `EV_MSC`, which is the whole reason [`Profile::AbsolutePointer`] advertises
/// one `MSC_SCAN` bit it never sends.
pub fn joydev_sees_an_absolute_mouse(profile: Profile) -> bool {
    let types = advertised_event_types(profile);
    let ev_match = [
        vec![ev::SYN, ev::KEY, ev::ABS],
        vec![ev::SYN, ev::KEY, ev::ABS, ev::MSC],
        vec![ev::SYN, ev::KEY, ev::ABS, ev::MSC, ev::REL],
    ]
    .into_iter()
    .any(|mut accepted| {
        accepted.sort_unstable();
        accepted == types
    });
    if !ev_match {
        return false;
    }
    if advertised_codes(profile, ev::ABS, 0x40) != vec![abs::X, abs::Y] {
        return false;
    }
    if advertised_codes(profile, ev::KEY, 0x300) != vec![btn::LEFT, btn::RIGHT, btn::MIDDLE] {
        return false;
    }
    // `BUS_AMIGA` (0x11) is the rule's one exception; ours is `BUS_VIRTUAL`.
    profile.devids().bustype != 0x11
}

/// Whether `joydev` would bind this profile — id table, then
/// [`joydev_sees_an_absolute_mouse`].
///
/// A device this returns `true` for gets a `/dev/input/js*` node; which number
/// it gets is the order the devices were registered in, which is the order the
/// machine attaches them.
pub fn joydev_would_bind(profile: Profile) -> bool {
    let has_abs = |code| profile.event_bits(ev::ABS).contains(code);
    let has_key = |code| profile.event_bits(ev::KEY).contains(code);
    let id_match = has_abs(abs::X)
        || has_abs(abs::Z)
        || has_abs(ABS_WHEEL)
        || has_abs(ABS_THROTTLE)
        || has_key(BTN_JOYSTICK)
        || has_key(BTN_GAMEPAD)
        || has_key(BTN_TRIGGER_HAPPY);
    // `joydev_dev_is_blacklisted()` rejects accelerometers, which are
    // recognised by `INPUT_PROP_ACCELEROMETER`; no profile publishes any
    // `INPUT_PROP_*` at all (`VIRTIO_INPUT_CFG_PROP_BITS` answers `size = 0`).
    id_match && !joydev_sees_an_absolute_mouse(profile)
}

/// `struct virtio_input_absinfo` on the wire.
///
/// The five fields are `__u32` in `virtio_input.h` but `__s32` in
/// `linux/input.h`, and a gamepad stick really does start at -32768 — so the
/// arguments are signed and the wire form is the two's-complement bit pattern,
/// which is exactly what the guest re-reads as `__s32`.
fn abs_info_bytes(min: i32, max: i32, fuzz: i32, flat: i32, res: i32) -> [u8; ABS_INFO_LEN] {
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
    selection_with_serial(profile, None, select, subsel)
}

/// [`selection`], plus the device-level serial the profile cannot know about.
///
/// Split out rather than folded into [`Profile`] because the serial is the one
/// piece of a device's identity that is *per instance*: two gamepads are the
/// same profile and differ only here.
pub fn selection_with_serial(
    profile: Profile,
    serial: Option<&str>,
    select: u8,
    subsel: u8,
) -> Selection {
    match select {
        VIRTIO_INPUT_CFG_ID_NAME => Selection::from_slice(profile.name().as_bytes()),
        VIRTIO_INPUT_CFG_ID_SERIAL => match serial {
            Some(serial) => Selection::from_slice(serial.as_bytes()),
            None => Selection::EMPTY,
        },
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
        assert_eq!(keyboard.payload(), b"Entangled Keyboard");
        assert_eq!(keyboard.size(), 18);

        let tablet = selection(Profile::AbsolutePointer, VIRTIO_INPUT_CFG_ID_NAME, 0);
        assert_eq!(tablet.payload(), b"Entangled Tablet");
        assert_eq!(tablet.size(), 16);

        // subsel is irrelevant for the name and must not change the answer.
        for subsel in [0u8, 1, 0x11, 0xff] {
            assert_eq!(
                selection(Profile::Keyboard, VIRTIO_INPUT_CFG_ID_NAME, subsel).payload(),
                b"Entangled Keyboard"
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
        for code in [key::RESERVED, 256, 271, btn::LEFT, btn::SIDE, 352, 354, 700] {
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
    fn pointer_key_bitmap_carries_exactly_the_three_primary_buttons() {
        let bits = selection(
            Profile::AbsolutePointer,
            VIRTIO_INPUT_CFG_EV_BITS,
            ev::KEY as u8,
        );
        // BTN_LEFT = 0x110 = 272 => byte 34, bit 0; BTN_MIDDLE = 0x112 => bit 2.
        assert_eq!(bits.size(), 35);
        let payload = bits.payload();
        assert_eq!(&payload[..34], &[0u8; 34]);
        assert_eq!(payload[34], 0x07);
        assert!(bit(payload, btn::LEFT));
        assert_eq!(payload[34] & 1, 1);
        for code in [btn::LEFT, btn::RIGHT, btn::MIDDLE] {
            assert!(bit(payload, code), "BTN code {code:#x} must be advertised");
        }
        // The two that had to go so `joydev` would leave the tablet alone.
        for code in [30u16, btn::SIDE, btn::EXTRA, 0x115, 0x116, 0x10f] {
            assert!(!bit(payload, code), "code {code:#x} must not be advertised");
        }
    }

    #[test]
    fn pointer_advertises_one_msc_bit_and_nothing_it_will_ever_send() {
        // `EV_MSC` exists on this profile purely to reach joydev's third
        // accepted event-type set (`SYN|KEY|ABS|MSC|REL`); a scroll wheel
        // cannot be had without it. One bit is enough — the driver sets
        // `EV_MSC` on any non-zero size.
        let bits = selection(
            Profile::AbsolutePointer,
            VIRTIO_INPUT_CFG_EV_BITS,
            ev::MSC as u8,
        );
        assert_eq!(bits.size(), 1);
        assert_eq!(bits.payload(), &[1 << msc::SCAN]);
        // It is the only `MSC_*` code the profile will admit: nothing on the
        // host produces one, so this is a capability, not a promise.
        assert!(bits.contains(msc::SCAN));
        for code in [0u16, 1, 2, 3, 5, 6, 0x20] {
            assert!(!bits.contains(code), "MSC {code:#x} must not be advertised");
        }
        assert!(selection(Profile::Keyboard, VIRTIO_INPUT_CFG_EV_BITS, ev::MSC as u8).is_empty());
        assert!(selection(Profile::Gamepad, VIRTIO_INPUT_CFG_EV_BITS, ev::MSC as u8).is_empty());
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
        // `EV_MSC` is the one exception and it has its own test; everything
        // else a pointer could plausibly claim stays absent.
        for subsel in [ev::REP, ev::LED, ev::SND, ev::SW] {
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
        for profile in Profile::ALL {
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
        for profile in Profile::ALL {
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

    // --------------------------------------------------------- gamepad bits

    /// `xpad`'s own capability set, spelled out so a change to either list is
    /// a diff a reviewer can check against `drivers/input/joystick/xpad.c`.
    const XPAD_BUTTONS: [u16; 11] = [
        0x130, 0x131, 0x133, 0x134, 0x136, 0x137, 0x13a, 0x13b, 0x13c, 0x13d, 0x13e,
    ];
    const XPAD_AXES: [u16; 8] = [0x00, 0x01, 0x02, 0x03, 0x04, 0x05, 0x10, 0x11];

    #[test]
    fn gamepad_key_bitmap_is_exactly_the_xpad_button_set() {
        let bits = selection(Profile::Gamepad, VIRTIO_INPUT_CFG_EV_BITS, ev::KEY as u8);
        // BTN_THUMBR = 0x13e = 318 => byte 39, bit 6, so 40 bytes of bitmap.
        assert_eq!(bits.size(), 40);
        let payload = bits.payload();
        assert_eq!(btn::GAMEPAD, XPAD_BUTTONS);
        for code in XPAD_BUTTONS {
            assert!(bit(payload, code), "BTN {code:#x} must be advertised");
        }
        // Everything below the BTN_GAMEPAD block is clear: no keyboard keys,
        // and — critically for `joydev_match` and udev — no mouse buttons.
        assert_eq!(&payload[..38], &[0u8; 38]);
        // 0x130..=0x137 is byte 38; BTN_C (0x132) and 0x135 stay clear.
        assert_eq!(payload[38], 0b1101_1011);
        // 0x138..=0x13f is byte 39; BTN_TL2/TR2 (0x138/0x139) stay clear.
        assert_eq!(payload[39], 0b0111_1100);
        for code in [
            btn::LEFT,
            btn::RIGHT,
            btn::MIDDLE,
            0x132, // BTN_C — a sixth face button changes SDL's guess
            0x135, // BTN_Z
            0x138, // BTN_TL2
            0x139, // BTN_TR2
            0x13f, // BTN_DPAD_UP: this pad uses the hat instead
            0x14a, // BTN_TOUCH: would make joydev refuse the device
            0x140, // BTN_DIGI: same
            30,    // KEY_A
        ] {
            assert!(!bit(payload, code), "code {code:#x} must not be advertised");
        }
    }

    #[test]
    fn gamepad_abs_bitmap_is_two_sticks_two_triggers_and_one_hat() {
        let bits = selection(Profile::Gamepad, VIRTIO_INPUT_CFG_EV_BITS, ev::ABS as u8);
        // ABS_HAT0Y = 0x11 = 17 => byte 2, bit 1.
        assert_eq!(bits.size(), 3);
        assert_eq!(bits.payload(), &[0b0011_1111, 0x00, 0b0000_0011]);
        for axis in XPAD_AXES {
            assert!(
                bit(bits.payload(), axis),
                "ABS {axis:#x} must be advertised"
            );
        }
        // No second hat, no ABS_THROTTLE/RUDDER, no ABS_WHEEL.
        for axis in [0x06u16, 0x07, 0x08, 0x12, 0x13, 0x14, 0x15] {
            assert!(!bit(bits.payload(), axis), "ABS {axis:#x} must be absent");
        }
    }

    #[test]
    fn gamepad_abs_info_matches_xpad_ranges_including_negative_minima() {
        let info = |axis: u16| {
            let s = selection(Profile::Gamepad, VIRTIO_INPUT_CFG_ABS_INFO, axis as u8);
            assert_eq!(s.size(), 20, "axis {axis:#x} must answer with an absinfo");
            let field = |n: usize| {
                i32::from_le_bytes([
                    s.payload()[n * 4],
                    s.payload()[n * 4 + 1],
                    s.payload()[n * 4 + 2],
                    s.payload()[n * 4 + 3],
                ])
            };
            (field(0), field(1), field(2), field(3), field(4))
        };

        for axis in [abs::X, abs::Y, abs::RX, abs::RY] {
            assert_eq!(info(axis), (-32768, 32767, 16, 128, 0), "stick {axis:#x}");
        }
        for axis in [abs::Z, abs::RZ] {
            assert_eq!(info(axis), (0, 255, 0, 0, 0), "trigger {axis:#x}");
        }
        for axis in [abs::HAT0X, abs::HAT0Y] {
            assert_eq!(info(axis), (-1, 1, 0, 0, 0), "hat {axis:#x}");
        }

        // The negative minimum really is on the wire as two's complement —
        // this is the byte pattern a guest re-reads as `__s32 = -32768`.
        let x = selection(Profile::Gamepad, VIRTIO_INPUT_CFG_ABS_INFO, abs::X as u8);
        assert_eq!(&x.payload()[0..4], &[0x00, 0x80, 0xff, 0xff]);
        assert_eq!(&x.payload()[4..8], &[0xff, 0x7f, 0x00, 0x00]);

        // Every axis the pad does not have answers size 0, so a driver never
        // registers an axis nothing will ever move.
        for axis in [0x06u8, 0x07, 0x08, 0x12, 0x28, 0x3f, 0xff] {
            assert!(selection(Profile::Gamepad, VIRTIO_INPUT_CFG_ABS_INFO, axis).is_empty());
        }
    }

    #[test]
    fn gamepad_advertises_nothing_else() {
        for subsel in [ev::REL, ev::REP, ev::MSC, ev::LED, ev::SND, ev::SW] {
            assert!(
                selection(Profile::Gamepad, VIRTIO_INPUT_CFG_EV_BITS, subsel as u8).is_empty(),
                "gamepad must not advertise event type {subsel}"
            );
        }
        // EV_FF (0x15) in particular: rumble is a follow-up, and a device that
        // claims force feedback it cannot deliver hangs SDL's upload.
        assert!(selection(Profile::Gamepad, VIRTIO_INPUT_CFG_EV_BITS, 0x15).is_empty());
    }

    #[test]
    fn the_three_devices_are_distinguishable_and_named() {
        let ids: Vec<_> = Profile::ALL.map(|p| p.devids().product).to_vec();
        assert_eq!(ids, vec![PRODUCT_KEYBOARD, PRODUCT_TABLET, PRODUCT_GAMEPAD]);
        let names: Vec<_> = Profile::ALL.map(|p| p.name()).to_vec();
        assert_eq!(
            names,
            vec![
                "Entangled Keyboard",
                "Entangled Tablet",
                "Entangled Gamepad"
            ]
        );
        for profile in Profile::ALL {
            let devids = profile.devids();
            assert_eq!(devids.bustype, BUS_VIRTUAL);
            assert_eq!(devids.vendor, VMHOST_INPUT_VENDOR);
            // Never Microsoft's 045e: the pad is auto-mapped from its
            // capabilities, not by impersonating hardware.
            assert_ne!(devids.vendor, 0x045e);
        }
    }

    #[test]
    fn gamepad_accepts_only_its_own_events_and_no_other_profile_claims_them() {
        let key = |code| InputEvent {
            event_type: ev::KEY,
            code,
            value: 1,
        };
        let axis = |code, value: i32| InputEvent {
            event_type: ev::ABS,
            code,
            value: value as u32,
        };
        for code in btn::GAMEPAD {
            assert!(Profile::Gamepad.accepts(key(code)));
            assert!(!Profile::Keyboard.accepts(key(code)));
            assert!(!Profile::AbsolutePointer.accepts(key(code)));
        }
        for code in [abs::Z, abs::RX, abs::RY, abs::RZ, abs::HAT0X, abs::HAT0Y] {
            assert!(Profile::Gamepad.accepts(axis(code, 0)));
            assert!(!Profile::AbsolutePointer.accepts(axis(code, 0)));
        }
        // ABS_X/ABS_Y are the one overlap with the tablet, and they mean
        // different things on the two devices — which is why gamepad events
        // never go through `split_batch` (see its docs).
        assert!(Profile::Gamepad.accepts(axis(abs::X, -32768)));
        assert!(Profile::AbsolutePointer.accepts(axis(abs::X, 0)));
        assert!(Profile::Gamepad.accepts(InputEvent::SYN_REPORT));
        assert!(!Profile::Gamepad.accepts(key(30)));
        assert!(!Profile::Gamepad.accepts(key(btn::LEFT)));
    }

    #[test]
    fn split_batch_never_routes_anything_to_the_gamepad() {
        // Host window capture feeds the keyboard and the tablet only; the pad
        // is fed by its own capture thread. Nothing here should change if a
        // window event happens to carry a code the pad also advertises.
        let batch = vec![
            InputEvent {
                event_type: ev::KEY,
                code: btn::SOUTH,
                value: 1,
            },
            InputEvent::SYN_REPORT,
        ];
        let split = crate::split_batch(&batch);
        assert!(split.keyboard.is_empty());
        assert!(split.pointer.is_empty());
    }
    // ------------------------------------------------------- joydev verdict

    #[test]
    fn the_driver_will_set_exactly_these_event_types() {
        // What `virtio_input.c` ends up with in `dev->evbit`, which is the
        // input to every classification below. `EV_SYN` is implicit; the rest
        // are set only where the `EV_BITS` selector answers non-empty.
        assert_eq!(
            advertised_event_types(Profile::Keyboard),
            vec![ev::SYN, ev::KEY, ev::REP]
        );
        assert_eq!(
            advertised_event_types(Profile::AbsolutePointer),
            vec![ev::SYN, ev::KEY, ev::REL, ev::ABS, ev::MSC]
        );
        assert_eq!(
            advertised_event_types(Profile::Gamepad),
            vec![ev::SYN, ev::KEY, ev::ABS]
        );
    }

    #[test]
    fn the_tablet_is_an_absolute_mouse_and_the_pad_is_a_joystick() {
        // The whole point of the tablet's shape (GAME-2104 follow-up): joydev
        // must refuse it, so the first pad gets `/dev/input/js0`.
        assert!(joydev_sees_an_absolute_mouse(Profile::AbsolutePointer));
        assert!(!joydev_would_bind(Profile::AbsolutePointer));

        assert!(!joydev_sees_an_absolute_mouse(Profile::Gamepad));
        assert!(joydev_would_bind(Profile::Gamepad));

        // A keyboard was never a candidate: no absolute axes, no joystick keys.
        assert!(!joydev_sees_an_absolute_mouse(Profile::Keyboard));
        assert!(!joydev_would_bind(Profile::Keyboard));
    }

    #[test]
    fn the_tablet_only_escapes_joydev_because_of_ev_msc() {
        // A regression guard with a name: drop `EV_MSC` and the escape hatch
        // closes again, because joydev accepts `EV_REL` only in its company.
        // Modelled by re-running the rule on the event-type set the pointer
        // would have without it.
        let mut without_msc = advertised_event_types(Profile::AbsolutePointer);
        without_msc.retain(|&t| t != ev::MSC);
        assert_eq!(without_msc, vec![ev::SYN, ev::KEY, ev::REL, ev::ABS]);
        for accepted in [
            vec![ev::SYN, ev::KEY, ev::ABS],
            vec![ev::SYN, ev::KEY, ev::ABS, ev::MSC],
            vec![ev::SYN, ev::KEY, ev::ABS, ev::MSC, ev::REL],
        ] {
            let mut accepted = accepted;
            accepted.sort_unstable();
            assert_ne!(
                accepted, without_msc,
                "a wheeled pointer without EV_MSC matches none of joydev's three sets"
            );
        }
    }

    #[test]
    fn every_profile_has_a_stable_joydev_verdict() {
        // One line per profile, so a capability change that flips a verdict
        // shows up as this test rather than as a guest boot three days later.
        let verdicts: Vec<(&str, bool)> = Profile::ALL
            .iter()
            .map(|&p| (p.name(), joydev_would_bind(p)))
            .collect();
        assert_eq!(
            verdicts,
            vec![
                ("Entangled Keyboard", false),
                ("Entangled Tablet", false),
                ("Entangled Gamepad", true),
            ]
        );
    }
}
