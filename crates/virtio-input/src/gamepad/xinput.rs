//! The Windows half of host gamepad capture: XInput.
//!
//! XInput is the right API here rather than raw HID or the newer
//! `Windows.Gaming.Input`, for one reason that decides it: **the guest device
//! is Xbox-shaped, and so is XInput**. Sticks are `SHORT`, triggers are
//! `BYTE`, and the button set is exactly the eleven the guest advertises minus
//! the guide button. So this file is almost entirely field copies — the only
//! real conversions are the D-pad (four bits → a hat) and the Y axes, which
//! XInput reports positive-up and evdev wants positive-down.
//!
//! # Polling, and the empty-slot trap
//!
//! There is no blocking XInput call and no hotplug notification, so this
//! polls. `XInputGetState` on a slot with no controller in it is famously slow
//! (it can reach the driver stack), and calling it for all four slots every
//! 8 ms is a real cost on a machine with one pad or none. So: while a
//! controller is adopted only *its* slot is polled, and empty slots are swept
//! at most once every [`RESCAN_INTERVAL`].
//!
//! # Which user index is which player
//!
//! Windows *does* have controller slots of its own, but they are not the same
//! thing as our players: XInput leaves a hole where a controller was unplugged
//! and hands a freshly plugged pad the lowest free index, so user 2 can be the
//! only pad on the machine. So the sweep offers whichever indices are occupied
//! to the shared [`PadRoster`], in ascending order, and the roster decides —
//! the same rule the evdev side uses, so a two-player VM behaves identically on
//! both hosts. With one player that is exactly the old behaviour: the lowest
//! occupied index.
//!
//! # What XInput cannot give us
//!
//! `BTN_MODE` (the guide/Xbox button) is bit `0x0400` of `wButtons` and
//! `XInputGetState` deliberately masks it out — Microsoft reserves it for the
//! Game Bar. The guest device still advertises the button, because SDL's
//! capability-based auto-mapping expects a complete pad and a controller
//! plugged into a *Linux* host does deliver it; on Windows it simply never
//! fires. Rumble (`XInputSetState`) is not here either, and will not be until
//! something can ask for it: no guest driver can reach `EV_FF` over
//! virtio-input and the spec has no channel to upload an effect through — the
//! full account, and what unblocking it needs, is on [`super`].

use std::sync::Arc;
use std::time::{Duration, Instant};

use windows::Win32::UI::Input::XboxController::{XInputGetState, XINPUT_STATE};

use super::{GamepadSource, PadId, PadRoster, PadState, Poll, RESCAN_INTERVAL};
use crate::btn;

/// XInput supports four controllers, which is also [`super::MAX_PLAYERS`].
const MAX_USERS: u32 = 4;

/// `ERROR_SUCCESS`.
const OK: u32 = 0;

/// `wButtons` bits, from `XInput.h`. Spelled out rather than imported so the
/// mapping table below reads as a table.
const DPAD_UP: u16 = 0x0001;
const DPAD_DOWN: u16 = 0x0002;
const DPAD_LEFT: u16 = 0x0004;
const DPAD_RIGHT: u16 = 0x0008;
const START: u16 = 0x0010;
const BACK: u16 = 0x0020;
const LEFT_THUMB: u16 = 0x0040;
const RIGHT_THUMB: u16 = 0x0080;
const LEFT_SHOULDER: u16 = 0x0100;
const RIGHT_SHOULDER: u16 = 0x0200;
const A: u16 = 0x1000;
const B: u16 = 0x2000;
const X: u16 = 0x4000;
const Y: u16 = 0x8000;

/// XInput button bit → the guest's `BTN_*` code. The whole mapping, in one
/// place, in the order [`btn::GAMEPAD`] uses.
const BUTTON_MAP: [(u16, u16); 10] = [
    (A, btn::SOUTH),
    (B, btn::EAST),
    (Y, btn::NORTH),
    (X, btn::WEST),
    (LEFT_SHOULDER, btn::TL),
    (RIGHT_SHOULDER, btn::TR),
    (BACK, btn::SELECT),
    (START, btn::START),
    (LEFT_THUMB, btn::THUMBL),
    (RIGHT_THUMB, btn::THUMBR),
];

/// Host gamepad capture through XInput.
pub struct XInputSource {
    /// Which player this source feeds (0-based).
    player: usize,
    /// Shared with every other player's source; see [`PadRoster`].
    roster: Arc<PadRoster>,
    /// Which user index is adopted, if any.
    user: Option<u32>,
    next_scan: Instant,
}

impl Default for XInputSource {
    fn default() -> Self {
        Self::new()
    }
}

impl XInputSource {
    /// A single-player source with a roster of its own.
    pub fn new() -> Self {
        Self::for_player(0, PadRoster::new(1))
    }

    /// The source for one player, sharing `roster` with the other players.
    pub fn for_player(player: usize, roster: Arc<PadRoster>) -> Self {
        Self {
            player,
            roster,
            user: None,
            // Sweep on the very first poll.
            next_scan: Instant::now(),
        }
    }

    /// The roster key of one XInput user index.
    fn slot_key(user: u32) -> String {
        format!("xinput-{user}")
    }

    /// One `XInputGetState` call. `None` means "nothing in that slot".
    fn read(user: u32) -> Option<XINPUT_STATE> {
        let mut state = XINPUT_STATE::default();
        // SAFETY: `XInputGetState` writes one `XINPUT_STATE` through the
        // pointer; `state` is exactly that, lives in local storage for the
        // whole call, and is a plain-old-data struct, so any bit pattern the
        // driver writes is a valid value. `user` is bounded by `MAX_USERS`
        // below, and an out-of-range index would be a return code, not a
        // memory error.
        let result = unsafe { XInputGetState(user, &mut state) };
        (result == OK).then_some(state)
    }

    /// Finds a controller, at most once per [`RESCAN_INTERVAL`].
    ///
    /// Every occupied index is offered, not the first one found: the roster
    /// cannot place a controller it has not been shown, and offering only the
    /// first would give every player the same pad.
    fn scan(&mut self) {
        if self.user.is_some() || Instant::now() < self.next_scan {
            return;
        }
        self.next_scan = Instant::now() + RESCAN_INTERVAL;
        let occupied: Vec<String> = (0..MAX_USERS)
            .filter(|&user| Self::read(user).is_some())
            .map(Self::slot_key)
            .collect();
        let Some(key) = self.roster.claim(self.player, &occupied) else {
            return;
        };
        let Some(user) = key
            .strip_prefix("xinput-")
            .and_then(|digits| digits.parse::<u32>().ok())
        else {
            return;
        };
        tracing::info!(
            user,
            player = self.player + 1,
            "adopted host gamepad (XInput)"
        );
        self.user = Some(user);
    }

    /// Drops the adopted controller and frees its roster slot.
    fn release(&mut self) {
        self.user = None;
        self.roster.release(self.player);
    }
}

impl Drop for XInputSource {
    /// A capture thread that stops — a device reset, or the VM closing —
    /// must not leave its player's roster slot claimed for ever.
    fn drop(&mut self) {
        self.roster.release(self.player);
    }
}

impl GamepadSource for XInputSource {
    fn name(&self) -> &'static str {
        "xinput"
    }

    fn poll(&mut self, timeout: Duration) -> Poll {
        self.scan();
        // XInput has no blocking read, so the pacing is ours. Sleeping first
        // rather than last means the state returned is the freshest one.
        std::thread::sleep(timeout);

        let Some(user) = self.user else {
            return Poll::Disconnected;
        };
        let Some(raw) = Self::read(user) else {
            tracing::debug!(
                user,
                player = self.player + 1,
                "host gamepad disconnected (XInput)"
            );
            self.release();
            return Poll::Disconnected;
        };
        Poll::Connected {
            id: PadId {
                label: format!("XInput controller {user}"),
                slot: u64::from(user),
            },
            state: translate(&raw),
        }
    }
}

/// `XINPUT_GAMEPAD` → [`PadState`].
///
/// Split out so the mapping is testable without a controller — which matters,
/// because two of its three decisions are easy to get backwards.
fn translate(raw: &XINPUT_STATE) -> PadState {
    let gamepad = raw.Gamepad;
    let buttons = gamepad.wButtons.0;
    let mut state = PadState::NEUTRAL;
    for (bit, code) in BUTTON_MAP {
        state.set_button_by_code(code, buttons & bit != 0);
    }
    // BTN_MODE is never set: `XInputGetState` masks the guide button out.

    // Decision 1: the D-pad. Both directions of an axis held at once is a
    // physical impossibility on a real pad but not on a lying driver, so it
    // resolves to centred rather than to whichever bit was tested last.
    state.hat = (
        axis_of(buttons & DPAD_LEFT != 0, buttons & DPAD_RIGHT != 0),
        axis_of(buttons & DPAD_UP != 0, buttons & DPAD_DOWN != 0),
    );

    // Decision 2: Y polarity. XInput reports sticks positive-**up**; evdev,
    // and therefore the guest device, wants positive-**down**. `!y` is the
    // same flip the kernel's own `xpad` driver applies, and unlike `-y` it
    // cannot overflow on i16::MIN.
    state.left_stick = (gamepad.sThumbLX, !gamepad.sThumbLY);
    state.right_stick = (gamepad.sThumbRX, !gamepad.sThumbRY);

    // Decision 3: none. Triggers are `BYTE` 0..=255 on both sides, which is
    // the whole reason the guest device advertises that range.
    state.left_trigger = gamepad.bLeftTrigger;
    state.right_trigger = gamepad.bRightTrigger;
    state
}

/// Two opposing digital directions → one hat component.
fn axis_of(negative: bool, positive: bool) -> i8 {
    match (negative, positive) {
        (true, false) => -1,
        (false, true) => 1,
        _ => 0,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use windows::Win32::UI::Input::XboxController::{XINPUT_GAMEPAD, XINPUT_GAMEPAD_BUTTON_FLAGS};

    fn raw(buttons: u16) -> XINPUT_STATE {
        XINPUT_STATE {
            dwPacketNumber: 1,
            Gamepad: XINPUT_GAMEPAD {
                wButtons: XINPUT_GAMEPAD_BUTTON_FLAGS(buttons),
                ..Default::default()
            },
        }
    }

    #[test]
    fn every_xinput_button_lands_on_the_button_the_guest_advertises() {
        for (bit, code) in BUTTON_MAP {
            let state = translate(&raw(bit));
            assert!(state.button_by_code(code), "bit {bit:#06x} -> {code:#x}");
            // …and nothing else moved.
            let pressed = state.buttons.iter().filter(|&&b| b).count();
            assert_eq!(pressed, 1, "bit {bit:#06x} pressed more than one button");
        }
        // The face buttons in particular: BTN_NORTH is Y and BTN_WEST is X,
        // which is the pairing that is wrong in half the mapping tables on the
        // internet.
        assert!(translate(&raw(Y)).button_by_code(btn::NORTH));
        assert!(translate(&raw(X)).button_by_code(btn::WEST));
        assert!(translate(&raw(A)).button_by_code(btn::SOUTH));
        assert!(translate(&raw(B)).button_by_code(btn::EAST));
    }

    #[test]
    fn the_guide_button_is_never_reported() {
        // 0x0400 is the guide bit; XInputGetState masks it, but even if a
        // driver leaked it we must not invent a press.
        let state = translate(&raw(0x0400));
        assert_eq!(state.buttons, PadState::NEUTRAL.buttons);
        assert_eq!(state.hat, (0, 0));
        assert!(!state.button_by_code(btn::MODE));
    }

    #[test]
    fn the_dpad_becomes_a_hat_and_opposing_directions_cancel() {
        assert_eq!(translate(&raw(DPAD_UP)).hat, (0, -1));
        assert_eq!(translate(&raw(DPAD_DOWN)).hat, (0, 1));
        assert_eq!(translate(&raw(DPAD_LEFT)).hat, (-1, 0));
        assert_eq!(translate(&raw(DPAD_RIGHT)).hat, (1, 0));
        assert_eq!(translate(&raw(DPAD_UP | DPAD_RIGHT)).hat, (1, -1));
        assert_eq!(translate(&raw(DPAD_LEFT | DPAD_RIGHT)).hat, (0, 0));
        assert_eq!(translate(&raw(DPAD_UP | DPAD_DOWN)).hat, (0, 0));
        assert_eq!(translate(&raw(0)).hat, (0, 0));
    }

    #[test]
    fn the_y_axes_are_flipped_and_the_extremes_survive_the_flip() {
        let with_sticks = |lx, ly, rx, ry| {
            let mut state = raw(0);
            state.Gamepad.sThumbLX = lx;
            state.Gamepad.sThumbLY = ly;
            state.Gamepad.sThumbRX = rx;
            state.Gamepad.sThumbRY = ry;
            translate(&state)
        };
        // Stick pushed fully up in XInput terms is fully *negative* in evdev
        // terms, which is what "up is -1" means for a hat too.
        let state = with_sticks(0, 32767, 0, -32768);
        assert_eq!(state.left_stick, (0, -32768));
        assert_eq!(state.right_stick, (0, 32767));
        // The one value a naive `-y` would overflow on.
        let state = with_sticks(-32768, -32768, 0, 0);
        assert_eq!(state.left_stick, (-32768, 32767));
        // Centred stays centred to within the one-count offset `!y` costs,
        // which is far inside the guest's advertised `flat` deadzone of 128.
        let state = with_sticks(0, 0, 0, 0);
        assert_eq!(state.left_stick, (0, -1));
        assert!(i32::from(state.left_stick.1).abs() < crate::pad::STICK_FLAT);
    }

    #[test]
    fn triggers_pass_through_unchanged() {
        let mut state = raw(0);
        state.Gamepad.bLeftTrigger = 0;
        state.Gamepad.bRightTrigger = 255;
        let state = translate(&state);
        assert_eq!((state.left_trigger, state.right_trigger), (0, 255));
    }

    #[test]
    fn a_source_with_no_controller_reports_disconnected() {
        // Passes with or without a pad plugged into this machine; what it
        // checks is that the source never claims a controller it did not read.
        let mut source = XInputSource::new();
        assert_eq!(source.name(), "xinput");
        match source.poll(Duration::from_millis(5)) {
            Poll::Disconnected => assert!(source.user.is_none()),
            Poll::Connected { id, .. } => {
                assert!(id.label.starts_with("XInput controller"));
                assert!(source.user.is_some());
            }
        }
    }
}
