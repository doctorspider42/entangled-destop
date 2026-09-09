//! virtio-input devices (backlog EPIC 9): a keyboard and an absolute pointer
//! (tablet-style, so the guest cursor tracks the host window 1:1 without
//! pointer grabs).
//!
//! Two things live here:
//!
//! * the Linux input event model the devices emit ([`InputEvent`] plus the
//!   [`ev`], [`key`], [`btn`], [`abs`], [`rel`] and [`rep`] code modules), which
//!   the host capture side (`display`) also uses;
//! * the [`InputDevice`] implementation of `virtio_core::VirtioDevice`, its
//!   configuration space ([`config`]) and the host-side event sink
//!   ([`InputHandle`]).
//!
//! # Integrator contract (MVP-901, wired up on `main`)
//!
//! One `InputDevice` per profile, each in its own virtio-mmio slot, plus one
//! [`InputHandle`] per device kept on the host side:
//!
//! ```no_run
//! use virtio_input::{split_batch, InputDevice};
//!
//! // Construction: two devices, two mmio slots.
//! let keyboard = InputDevice::keyboard();
//! let tablet = InputDevice::absolute_pointer();
//! // Host-side sinks, cheap to clone and `Send + Sync`; keep these before the
//! // devices are handed to `MmioTransport::new`, which takes ownership.
//! let keys = keyboard.handle();
//! let pointer = tablet.handle();
//!
//! // Event pump: `display::InputQueue::drain_batches()` produces
//! // SYN_REPORT-terminated batches. A batch may mix keyboard and pointer
//! // events (focus loss releases held keys *and* mouse buttons at once), so
//! // route per event, never per batch.
//! # let captured: Vec<Vec<virtio_input::InputEvent>> = Vec::new();
//! for batch in captured {
//!     let split = split_batch(&batch);
//!     if let Err(error) = keys.push(&split.keyboard) {
//!         tracing::warn!(%error, "keyboard event delivery failed");
//!     }
//!     if let Err(error) = pointer.push(&split.pointer) {
//!         tracing::warn!(%error, "pointer event delivery failed");
//!     }
//! }
//! ```
//!
//! [`InputHandle::push`] delivers straight into the guest's event queue when
//! buffers are available, so the host does not have to wait for a guest kick.
//! Events pushed before the driver sets `DRIVER_OK` are dropped, and events
//! pushed while the guest is not refilling the queue are buffered up to
//! [`MAX_PENDING_EVENTS`] (drop-oldest beyond that, counted in
//! [`EventStats`]).

pub mod config;
pub mod device;
pub mod gamepad;

pub use config::{DevIds, Profile, Selection};
pub use device::{
    BufferError, EventStats, InputDevice, InputHandle, StatusEvent, EV_FF, MAX_PENDING_EVENTS,
};
pub use gamepad::{
    open_source, open_sources, GamepadCapture, GamepadError, GamepadSource, PadId, PadRoster,
    PadState, Poll, SourceChoice, SourceFactory, MAX_PLAYERS,
};

/// Linux input event types (`EV_*` from `linux/input-event-codes.h`).
pub mod ev {
    pub const SYN: u16 = 0x00;
    pub const KEY: u16 = 0x01;
    pub const REL: u16 = 0x02;
    pub const ABS: u16 = 0x03;
    pub const MSC: u16 = 0x04;
    pub const SW: u16 = 0x05;
    pub const LED: u16 = 0x11;
    pub const SND: u16 = 0x12;
    pub const REP: u16 = 0x14;
    /// One past the highest event type (`EV_CNT`).
    pub const CNT: u16 = 0x20;
}

/// Keyboard key codes (`KEY_*`) the devices need by name. The keyboard profile
/// advertises the whole low range, so only the outliers are listed.
pub mod key {
    /// `KEY_RESERVED`: never sent, never advertised.
    pub const RESERVED: u16 = 0;
    /// Highest key code the keyboard profile advertises from the dense low
    /// range (`display`'s keymap stays inside it apart from [`SELECT`]).
    pub const DENSE_MAX: u16 = 255;
    /// `KEY_BACK` — where the host's Back mouse button lands (GAME-2104
    /// follow-up; see [`crate::btn::SIDE`] for why it is not a mouse button).
    pub const BACK: u16 = 158;
    /// `KEY_FORWARD` — the host's Forward mouse button.
    pub const FORWARD: u16 = 159;
    /// `KEY_SELECT`, the one code `display`'s keymap emits above
    /// [`DENSE_MAX`].
    pub const SELECT: u16 = 353;
}

/// Absolute axes (`ABS_*`). The pointer uses the first two; the gamepad uses
/// all of them, with the codes `xpad` (the in-tree Xbox controller driver)
/// reports — see [`Profile::Gamepad`](config::Profile::Gamepad).
pub mod abs {
    /// Left stick X on the gamepad, window X on the pointer.
    pub const X: u16 = 0x00;
    /// Left stick Y on the gamepad, window Y on the pointer.
    pub const Y: u16 = 0x01;
    /// `ABS_Z` — the **left** trigger, as `xpad` reports it.
    pub const Z: u16 = 0x02;
    /// `ABS_RX` — right stick X.
    pub const RX: u16 = 0x03;
    /// `ABS_RY` — right stick Y.
    pub const RY: u16 = 0x04;
    /// `ABS_RZ` — the **right** trigger.
    pub const RZ: u16 = 0x05;
    /// `ABS_HAT0X` — D-pad left/right, one of `-1`, `0`, `1`.
    pub const HAT0X: u16 = 0x10;
    /// `ABS_HAT0Y` — D-pad up/down; `-1` is *up*, following evdev convention.
    pub const HAT0Y: u16 = 0x11;
}

/// Miscellaneous events (`MSC_*`).
pub mod msc {
    /// `MSC_SCAN` — the raw scancode behind a key. Advertised by the pointer
    /// and never sent; see [`Profile::AbsolutePointer`](config::Profile::AbsolutePointer)
    /// for the one reason it is there.
    pub const SCAN: u16 = 0x04;
}

/// Relative axes; the absolute pointer still needs them for the scroll wheel.
pub mod rel {
    /// `REL_HWHEEL` — horizontal scroll, in notches.
    pub const HWHEEL: u16 = 0x06;
    /// `REL_WHEEL` — vertical scroll, in notches.
    pub const WHEEL: u16 = 0x08;
}

/// Mouse buttons (`BTN_*`). Linux carries these in the `EV_KEY` code space.
pub mod btn {
    pub const LEFT: u16 = 0x110;
    pub const RIGHT: u16 = 0x111;
    pub const MIDDLE: u16 = 0x112;
    /// `BTN_SIDE` — deliberately **not** advertised by any profile, and kept
    /// here so the reason is written next to the code.
    ///
    /// `joydev` refuses to bind an "absolute mouse", and its definition of one
    /// (`joydev_dev_is_absolute_mouse()`) demands a key set that is *exactly*
    /// `BTN_LEFT`/`BTN_RIGHT`/`BTN_MIDDLE`. Advertising these two extra
    /// buttons made the tablet a joystick, which cost the gamepad `js0`
    /// (GAME-2104 follow-up). The host's Back/Forward mouse buttons now go to
    /// the keyboard as `KEY_BACK`/`KEY_FORWARD`, which is what a multimedia
    /// keyboard sends and what browsers and desktops already bind.
    pub const SIDE: u16 = 0x113;
    /// `BTN_EXTRA` — not advertised either; see [`SIDE`].
    pub const EXTRA: u16 = 0x114;

    // ---- gamepad (`BTN_GAMEPAD` block, 0x130..=0x13e) ----
    //
    // Codes and names are the kernel's, so what the guest sees is what a real
    // Xbox-shaped pad on a Linux host sees. Note the two traps `xpad` also
    // has to live with: `BTN_NORTH` (0x133) is the *top* face button (Y on an
    // Xbox pad) and `BTN_WEST` (0x134) is the *left* one (X) — they are not in
    // clockwise order — and 0x132 (`BTN_C`) is deliberately skipped, because a
    // pad that advertises it is a six-face-button pad to SDL.
    /// `BTN_SOUTH` / `BTN_A` — the bottom face button.
    pub const SOUTH: u16 = 0x130;
    /// `BTN_EAST` / `BTN_B` — the right face button.
    pub const EAST: u16 = 0x131;
    /// `BTN_NORTH` / `BTN_Y` — the top face button.
    pub const NORTH: u16 = 0x133;
    /// `BTN_WEST` / `BTN_X` — the left face button.
    pub const WEST: u16 = 0x134;
    /// `BTN_TL` — left shoulder.
    pub const TL: u16 = 0x136;
    /// `BTN_TR` — right shoulder.
    pub const TR: u16 = 0x137;
    /// `BTN_SELECT` — "back" / "view".
    pub const SELECT: u16 = 0x13a;
    /// `BTN_START` — "start" / "menu".
    pub const START: u16 = 0x13b;
    /// `BTN_MODE` — the guide / logo button.
    pub const MODE: u16 = 0x13c;
    /// `BTN_THUMBL` — left stick click.
    pub const THUMBL: u16 = 0x13d;
    /// `BTN_THUMBR` — right stick click.
    pub const THUMBR: u16 = 0x13e;

    /// Every gamepad button, in ascending code order — the single list the
    /// config bitmap, the host capture and the tests all read from.
    pub const GAMEPAD: [u16; 11] = [
        SOUTH, EAST, NORTH, WEST, TL, TR, SELECT, START, MODE, THUMBL, THUMBR,
    ];
}

/// Auto-repeat parameters (`REP_*`). The guest's input core owns repeat
/// generation; the keyboard only advertises that it supports it.
pub mod rep {
    pub const DELAY: u16 = 0x00;
    pub const PERIOD: u16 = 0x01;
}

/// Range advertised for ABS_X/ABS_Y; host window coordinates are rescaled
/// into this range so guest position matches the window exactly (MVP-903).
pub const ABS_AXIS_MAX: u32 = 32767;

/// Axis geometry of the gamepad, byte-for-byte what the kernel's `xpad` driver
/// publishes for an Xbox controller (GAME-2104).
///
/// The numbers are copied rather than invented on purpose: userspace does not
/// read a pad's *name* to decide how to drive it, it reads these ranges. A
/// stick that ran 0..32767 or a trigger that ran -128..127 would still be a
/// working evdev device and SDL would still bind to it, but every guess it
/// makes about centre, polarity and full deflection would be wrong.
pub mod pad {
    /// Stick minimum (`xpad`: `input_set_abs_params(..., -32768, 32767, 16, 128)`).
    pub const STICK_MIN: i32 = -32768;
    /// Stick maximum.
    pub const STICK_MAX: i32 = 32767;
    /// Stick `fuzz`: the input core swallows changes smaller than this, which
    /// is noise filtering on real hardware and free on a synthetic pad.
    pub const STICK_FUZZ: i32 = 16;
    /// Stick `flat`: the **guest's** deadzone, in the guest's own units.
    ///
    /// This is the only deadzone in the whole path, and it is advertised
    /// rather than applied: the host forwards raw stick values and lets
    /// `joydev` (which honours `flat`) and SDL (which ignores it and uses its
    /// own) each do what they already do for a real pad.
    pub const STICK_FLAT: i32 = 128;
    /// Trigger minimum — triggers are unipolar, exactly like XInput's `BYTE`.
    pub const TRIGGER_MIN: i32 = 0;
    /// Trigger maximum.
    pub const TRIGGER_MAX: i32 = 255;
    /// Hat minimum (left / up).
    pub const HAT_MIN: i32 = -1;
    /// Hat maximum (right / down).
    pub const HAT_MAX: i32 = 1;
}

/// One event as carried in the virtio-input event queue (matches
/// `struct virtio_input_event`: all fields little-endian on the wire).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct InputEvent {
    pub event_type: u16,
    pub code: u16,
    pub value: u32,
}

impl InputEvent {
    /// Size of `struct virtio_input_event` on the wire: `type` (2) + `code` (2)
    /// + `value` (4). One event per descriptor chain, per the spec.
    pub const WIRE_SIZE: usize = 8;

    pub const SYN_REPORT: InputEvent = InputEvent {
        event_type: ev::SYN,
        code: 0,
        value: 0,
    };

    /// Scales a host window coordinate into the ABS axis range.
    pub fn abs_from_window(axis: u16, pos: f64, window_extent: f64) -> Self {
        let clamped = pos.clamp(0.0, window_extent);
        let value = if window_extent > 0.0 {
            ((clamped / window_extent) * f64::from(ABS_AXIS_MAX)).round() as u32
        } else {
            0
        };
        InputEvent {
            event_type: ev::ABS,
            code: axis,
            value,
        }
    }

    /// The guest-visible byte representation (little-endian, as the spec
    /// requires for the modern interface).
    pub fn to_le_bytes(self) -> [u8; Self::WIRE_SIZE] {
        let mut bytes = [0u8; Self::WIRE_SIZE];
        bytes[0..2].copy_from_slice(&self.event_type.to_le_bytes());
        bytes[2..4].copy_from_slice(&self.code.to_le_bytes());
        bytes[4..8].copy_from_slice(&self.value.to_le_bytes());
        bytes
    }

    /// Parses one wire event, as found in a status-queue buffer. Every bit
    /// pattern is a valid event, so this cannot fail.
    pub fn from_le_bytes(bytes: [u8; Self::WIRE_SIZE]) -> Self {
        InputEvent {
            event_type: u16::from_le_bytes([bytes[0], bytes[1]]),
            code: u16::from_le_bytes([bytes[2], bytes[3]]),
            value: u32::from_le_bytes([bytes[4], bytes[5], bytes[6], bytes[7]]),
        }
    }

    /// True for the `SYN_REPORT` that terminates every batch (MVP-905).
    pub fn is_syn_report(self) -> bool {
        self.event_type == ev::SYN && self.code == 0
    }
}

/// One captured batch, routed to the devices that advertise its events.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct SplitBatch {
    /// Events for the [`Profile::Keyboard`] device, `SYN_REPORT`-terminated.
    pub keyboard: Vec<InputEvent>,
    /// Events for the [`Profile::AbsolutePointer`] device,
    /// `SYN_REPORT`-terminated.
    pub pointer: Vec<InputEvent>,
}

/// Routes one host batch to the two devices, per event.
///
/// A batch is *not* uniform: `display` releases held keys and held mouse
/// buttons in a single batch when the window loses focus (MVP-906), and Linux
/// carries both in the `EV_KEY` code space. Routing whole batches would give
/// one device events it never advertised — and lose the other device's
/// key-ups. So each event goes to whichever profile advertises its
/// `(type, code)` pair ([`Profile::accepts`]), and each non-empty half is
/// terminated with its own `SYN_REPORT` so the guest sees complete reports.
pub fn split_batch(batch: &[InputEvent]) -> SplitBatch {
    let mut split = SplitBatch::default();
    for &event in batch {
        if event.event_type == ev::SYN {
            continue;
        }
        if Profile::Keyboard.accepts(event) {
            split.keyboard.push(event);
        }
        if Profile::AbsolutePointer.accepts(event) {
            split.pointer.push(event);
        }
    }
    if !split.keyboard.is_empty() {
        split.keyboard.push(InputEvent::SYN_REPORT);
    }
    if !split.pointer.is_empty() {
        split.pointer.push(InputEvent::SYN_REPORT);
    }
    split
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn abs_scaling_endpoints_and_center() {
        let e = InputEvent::abs_from_window(abs::X, 0.0, 1920.0);
        assert_eq!(e.value, 0);
        let e = InputEvent::abs_from_window(abs::X, 1920.0, 1920.0);
        assert_eq!(e.value, ABS_AXIS_MAX);
        let e = InputEvent::abs_from_window(abs::Y, 960.0, 1920.0);
        assert_eq!(e.value, ABS_AXIS_MAX / 2 + 1); // 16384, rounding up from 16383.5
    }

    #[test]
    fn out_of_window_positions_clamp() {
        assert_eq!(InputEvent::abs_from_window(abs::X, -5.0, 1920.0).value, 0);
        assert_eq!(
            InputEvent::abs_from_window(abs::X, 5000.0, 1920.0).value,
            ABS_AXIS_MAX
        );
        assert_eq!(InputEvent::abs_from_window(abs::X, 10.0, 0.0).value, 0);
    }

    #[test]
    fn wire_format_is_little_endian() {
        let event = InputEvent {
            event_type: ev::KEY,
            code: 30,
            value: 1,
        };
        assert_eq!(
            event.to_le_bytes(),
            [0x01, 0x00, 0x1e, 0x00, 0x01, 0x00, 0x00, 0x00]
        );
        // ABS_Y at the top of the axis range: 0x7fff.
        let event = InputEvent {
            event_type: ev::ABS,
            code: abs::Y,
            value: ABS_AXIS_MAX,
        };
        assert_eq!(
            event.to_le_bytes(),
            [0x03, 0x00, 0x01, 0x00, 0xff, 0x7f, 0x00, 0x00]
        );
        // A wheel notch backwards is a two's-complement -1.
        let event = InputEvent {
            event_type: ev::REL,
            code: rel::WHEEL,
            value: (-1i32) as u32,
        };
        assert_eq!(
            event.to_le_bytes(),
            [0x02, 0x00, 0x08, 0x00, 0xff, 0xff, 0xff, 0xff]
        );
        assert_eq!(InputEvent::SYN_REPORT.to_le_bytes(), [0u8; 8]);
    }

    #[test]
    fn wire_format_round_trips() {
        for event in [
            InputEvent::SYN_REPORT,
            InputEvent {
                event_type: ev::KEY,
                code: btn::LEFT,
                value: 1,
            },
            InputEvent {
                event_type: ev::LED,
                code: 1,
                value: u32::MAX,
            },
            InputEvent {
                event_type: u16::MAX,
                code: u16::MAX,
                value: u32::MAX,
            },
        ] {
            assert_eq!(InputEvent::from_le_bytes(event.to_le_bytes()), event);
        }
    }

    #[test]
    fn syn_report_is_recognised() {
        assert!(InputEvent::SYN_REPORT.is_syn_report());
        assert!(!InputEvent {
            event_type: ev::KEY,
            code: 30,
            value: 1
        }
        .is_syn_report());
    }

    #[test]
    fn split_batch_sends_keys_to_the_keyboard_and_pointer_events_to_the_tablet() {
        let batch = vec![
            InputEvent {
                event_type: ev::KEY,
                code: 30,
                value: 1,
            },
            InputEvent::SYN_REPORT,
        ];
        let split = split_batch(&batch);
        assert_eq!(split.keyboard, batch);
        assert!(split.pointer.is_empty());

        let batch = vec![
            InputEvent::abs_from_window(abs::X, 10.0, 100.0),
            InputEvent::abs_from_window(abs::Y, 20.0, 100.0),
            InputEvent::SYN_REPORT,
        ];
        let split = split_batch(&batch);
        assert!(split.keyboard.is_empty());
        assert_eq!(split.pointer, batch);
    }

    #[test]
    fn split_batch_separates_a_mixed_release_all_batch() {
        // What `display` emits on focus loss: held keys and held mouse buttons
        // in one batch (MVP-906).
        let up = |code| InputEvent {
            event_type: ev::KEY,
            code,
            value: 0,
        };
        let batch = vec![
            up(29),
            up(30),
            up(key::BACK),
            up(btn::LEFT),
            up(btn::MIDDLE),
            InputEvent::SYN_REPORT,
        ];
        let split = split_batch(&batch);
        assert_eq!(
            split.keyboard,
            vec![up(29), up(30), up(key::BACK), InputEvent::SYN_REPORT],
            "the keyboard must still see its own key-ups, Back among them"
        );
        assert_eq!(
            split.pointer,
            vec![up(btn::LEFT), up(btn::MIDDLE), InputEvent::SYN_REPORT]
        );
    }

    #[test]
    fn split_batch_drops_events_neither_device_advertises() {
        let batch = vec![
            // MSC_TIMESTAMP: the pointer advertises MSC_SCAN and nothing
            // else, so this one still belongs to nobody.
            InputEvent {
                event_type: ev::MSC,
                code: 5,
                value: 7,
            },
            InputEvent {
                event_type: ev::KEY,
                code: 900,
                value: 1,
            },
            // BTN_SIDE: no profile advertises it any more (GAME-2104
            // follow-up), so it must be dropped rather than delivered to a
            // device that would never have registered the code.
            InputEvent {
                event_type: ev::KEY,
                code: btn::SIDE,
                value: 1,
            },
            InputEvent {
                event_type: ev::KEY,
                code: key::RESERVED,
                value: 1,
            },
            InputEvent::SYN_REPORT,
        ];
        let split = split_batch(&batch);
        assert!(split.keyboard.is_empty());
        assert!(split.pointer.is_empty());

        // A SYN-only batch produces nothing at all.
        assert_eq!(
            split_batch(&[InputEvent::SYN_REPORT]),
            SplitBatch::default()
        );
        assert_eq!(split_batch(&[]), SplitBatch::default());
    }

    #[test]
    fn split_batch_routes_the_wheel_to_the_pointer() {
        let batch = vec![
            InputEvent {
                event_type: ev::REL,
                code: rel::WHEEL,
                value: 1,
            },
            InputEvent {
                event_type: ev::REL,
                code: rel::HWHEEL,
                value: (-2i32) as u32,
            },
            InputEvent::SYN_REPORT,
        ];
        let split = split_batch(&batch);
        assert!(split.keyboard.is_empty());
        assert_eq!(split.pointer.len(), 3);
        assert_eq!(
            *split.pointer.last().expect("terminated"),
            InputEvent::SYN_REPORT
        );
    }
}
