//! Host gamepad capture (backlog GAME-2104): a real controller on the host
//! driving the guest's [`Profile::Gamepad`](crate::config::Profile::Gamepad)
//! device.
//!
//! # Shape of the path
//!
//! ```text
//!   host controller ──▶ GamepadSource::poll ──▶ PadState ──┐
//!                        (evdev / XInput)                  │ delta()
//!                                                          ▼
//!   guest /dev/input/js0 ◀── virtio eventq ◀── InputHandle::push
//! ```
//!
//! The middle of that chain is the interesting part, and it is deliberately
//! **state-based rather than event-based**. A source reports the pad's
//! *complete current state*; [`GamepadCapture`] diffs it against what the
//! guest was last told and emits only the difference. Three things fall out
//! of that which an event-forwarding design has to build by hand:
//!
//! * **Hotplug is free in both directions.** A controller that disappears is
//!   just [`Poll::Disconnected`], which the pump treats as
//!   [`PadState::NEUTRAL`] — so held buttons are released, sticks re-centre
//!   and the guest is left in a sane state instead of running forward for
//!   ever. And because the neutral state then equals the last state, an
//!   absent controller produces exactly **zero** events per tick rather than
//!   a stream of nothing.
//! * **A different controller is not a special case.** Unplug pad A, plug in
//!   pad B: the pump sees a new [`Poll::id`], but all it *does* is diff, so
//!   the guest sees whichever buttons actually differ.
//! * **A paused VM cannot accumulate a burst.** The events that would have
//!   been produced while the pump is parked on the pause gate collapse into
//!   the one delta computed after it resumes.
//!
//! # Deadzones and curves
//!
//! There are none here, on purpose. The host rescales a source's raw axis
//! range onto the range the guest device advertises — a unit conversion, not
//! a filter — and stops. The only deadzone in the whole path is the `flat`
//! value in the guest's own `ABS_INFO` ([`crate::pad::STICK_FLAT`]), which is
//! `xpad`'s, applied by whatever in the guest already applies it for real
//! hardware. A host-side curve would be invisible to the guest's own
//! calibration and impossible for a game to undo.

use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::Arc;
use std::thread::JoinHandle;
use std::time::Duration;

use thiserror::Error;
use virtio_core::Quiesce;

use crate::{abs, btn, ev, pad, InputEvent, InputHandle};

#[cfg(target_os = "linux")]
pub mod evdev;
#[cfg(windows)]
pub mod xinput;

/// How often the pump asks its source for the controller's state: 125 Hz, the
/// report rate of an ordinary USB pad. Also the longest a pause can wait for
/// this thread to reach the gate.
pub const POLL_INTERVAL: Duration = Duration::from_millis(8);

/// How often a source looks for a controller that was not there before. One
/// second is imperceptible for a plug-in and keeps the cost of *not* having a
/// pad down to one directory scan (Linux) or four `XInputGetState` calls
/// (Windows) per second — the latter matters, because probing an empty XInput
/// slot is documented as slow.
pub const RESCAN_INTERVAL: Duration = Duration::from_secs(1);

/// Buttons on the pad, in the order [`btn::GAMEPAD`] lists them.
pub const BUTTON_COUNT: usize = btn::GAMEPAD.len();

/// The largest report the pump can produce in one tick: every button changing
/// (11) plus every axis changing (8) plus the terminating `SYN_REPORT`.
///
/// A bound worth naming even though nothing guest-controlled feeds it — it is
/// what makes "one host tick is one bounded push" true by construction, so the
/// only thing that can grow without bound behind a stalled guest is
/// [`crate::MAX_PENDING_EVENTS`], which already drops the oldest.
pub const MAX_EVENTS_PER_REPORT: usize = BUTTON_COUNT + 8 + 1;

/// Where a [`GamepadSource`] comes from.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum SourceChoice {
    /// The host's native mechanism if this OS has one, [`Self::Null`] if not.
    /// A machine with no controller attached is not an error — that is what
    /// hotplug is for — so this never fails.
    #[default]
    Auto,
    /// A pad that is never connected. What a headless or CI run wants: the
    /// guest still enumerates a working joystick, and the tests still drive it
    /// through [`InputHandle::push`].
    Null,
    /// Linux: `/dev/input/event*` read directly.
    Evdev,
    /// Windows: XInput.
    XInput,
}

impl std::fmt::Display for SourceChoice {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(match self {
            Self::Auto => "auto",
            Self::Null => "null",
            Self::Evdev => "evdev",
            Self::XInput => "xinput",
        })
    }
}

#[derive(Debug, Error)]
pub enum GamepadError {
    #[error("gamepad source \"{choice}\" is not available on this host: {reason}")]
    Unavailable {
        choice: SourceChoice,
        reason: String,
    },

    #[error("cannot spawn the gamepad capture thread: {0}")]
    Spawn(String),
}

/// Makes a fresh [`GamepadSource`].
///
/// A factory rather than a source, because a device can be activated more than
/// once (a guest reset re-activates it) and each activation gets its own
/// capture thread with its own file descriptors. Reusing one source across
/// activations would mean handing a `&mut` across a thread boundary that has
/// already been joined once.
pub type SourceFactory = Arc<dyn Fn() -> Box<dyn GamepadSource> + Send + Sync>;

/// One controller's complete state, already in the units the guest device
/// advertises ([`crate::pad`]).
///
/// Copy and comparable on purpose: the pump keeps two of them and the whole
/// diffing logic is `previous != current`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct PadState {
    /// Indexed the same way [`btn::GAMEPAD`] is.
    pub buttons: [bool; BUTTON_COUNT],
    /// Left stick, `-32768..=32767`, Y positive **down** (evdev convention).
    pub left_stick: (i16, i16),
    /// Right stick, same convention.
    pub right_stick: (i16, i16),
    /// Left trigger, `0..=255`.
    pub left_trigger: u8,
    /// Right trigger, `0..=255`.
    pub right_trigger: u8,
    /// D-pad as a hat: each component is `-1`, `0` or `1`, Y negative **up**.
    pub hat: (i8, i8),
}

impl Default for PadState {
    fn default() -> Self {
        Self::NEUTRAL
    }
}

impl PadState {
    /// Nothing pressed, sticks centred, triggers released. Also what a
    /// disconnected controller reports, which is what makes an unplug safe.
    pub const NEUTRAL: PadState = PadState {
        buttons: [false; BUTTON_COUNT],
        left_stick: (0, 0),
        right_stick: (0, 0),
        left_trigger: 0,
        right_trigger: 0,
        hat: (0, 0),
    };

    /// Sets one button by its `BTN_*` code, ignoring codes this pad has no
    /// slot for. Used by the evdev source, which is fed codes by the host
    /// kernel and must not index an array with them.
    pub fn set_button_by_code(&mut self, code: u16, pressed: bool) -> bool {
        match btn::GAMEPAD.iter().position(|&c| c == code) {
            Some(index) => {
                self.buttons[index] = pressed;
                true
            }
            None => false,
        }
    }

    /// Reads one button by its `BTN_*` code.
    pub fn button_by_code(&self, code: u16) -> bool {
        btn::GAMEPAD
            .iter()
            .position(|&c| c == code)
            .is_some_and(|index| self.buttons[index])
    }

    /// The events that move a guest holding `previous` to holding `self`,
    /// terminated with a `SYN_REPORT`. Empty when nothing changed — including
    /// the terminator, because a report with no events in it is noise.
    ///
    /// At most [`MAX_EVENTS_PER_REPORT`] events, and every `(type, code)` pair
    /// is one the gamepad profile advertises (there is a test).
    pub fn delta(&self, previous: &PadState) -> Vec<InputEvent> {
        let mut events = Vec::new();
        // Buttons before axes, matching the order a real pad's report is
        // decoded in; nothing depends on it, but a stable order makes the
        // round-trip tests readable.
        for (index, &code) in btn::GAMEPAD.iter().enumerate() {
            if self.buttons[index] != previous.buttons[index] {
                events.push(InputEvent {
                    event_type: ev::KEY,
                    code,
                    value: u32::from(self.buttons[index]),
                });
            }
        }
        let mut axis = |code: u16, now: i32, before: i32| {
            if now != before {
                events.push(InputEvent {
                    event_type: ev::ABS,
                    code,
                    value: now as u32,
                });
            }
        };
        axis(
            abs::X,
            i32::from(self.left_stick.0),
            i32::from(previous.left_stick.0),
        );
        axis(
            abs::Y,
            i32::from(self.left_stick.1),
            i32::from(previous.left_stick.1),
        );
        axis(
            abs::RX,
            i32::from(self.right_stick.0),
            i32::from(previous.right_stick.0),
        );
        axis(
            abs::RY,
            i32::from(self.right_stick.1),
            i32::from(previous.right_stick.1),
        );
        axis(
            abs::Z,
            i32::from(self.left_trigger),
            i32::from(previous.left_trigger),
        );
        axis(
            abs::RZ,
            i32::from(self.right_trigger),
            i32::from(previous.right_trigger),
        );
        axis(abs::HAT0X, i32::from(self.hat.0), i32::from(previous.hat.0));
        axis(abs::HAT0Y, i32::from(self.hat.1), i32::from(previous.hat.1));
        if !events.is_empty() {
            events.push(InputEvent::SYN_REPORT);
        }
        events
    }
}

/// What one poll of a host controller found.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Poll {
    /// A controller is attached and this is its state. `id` identifies *which*
    /// controller, so the pump can log a swap; it is opaque and only ever
    /// compared for equality.
    Connected { id: PadId, state: PadState },
    /// Nothing attached. Never an error: an unplugged pad is the normal state
    /// of most machines.
    Disconnected,
}

/// Opaque identity of a host controller, stable while it stays plugged in.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PadId {
    /// Something a human can read in a log line: an evdev device name, or
    /// `XInput user 0`.
    pub label: String,
    /// Distinguishes two controllers with identical labels.
    pub slot: u64,
}

/// A host mechanism that can be asked for a controller's state.
///
/// Implementations may block for up to the interval they are given, which is
/// what lets the evdev source sit in `poll(2)` instead of spinning.
pub trait GamepadSource: Send {
    /// The mechanism's name, for logs: `evdev`, `xinput`, `null`.
    fn name(&self) -> &'static str;

    /// Looks for input, blocking for at most `timeout`.
    fn poll(&mut self, timeout: Duration) -> Poll;
}

/// A source with no controller behind it, on every OS.
#[derive(Debug, Default)]
pub struct NullSource;

impl GamepadSource for NullSource {
    fn name(&self) -> &'static str {
        "null"
    }

    fn poll(&mut self, timeout: Duration) -> Poll {
        // Still sleeps: the pump's cadence is the source's business, and a
        // null source that returned instantly would spin a core.
        std::thread::sleep(timeout);
        Poll::Disconnected
    }
}

/// Picks a host mechanism, returning its name and a factory for it.
///
/// Mirrors `virtio_sound::open_sink`: `auto` never fails (a VM must not refuse
/// to boot because this machine has no controller), while an explicitly named
/// mechanism that is not there fails the run rather than silently doing
/// nothing.
pub fn open_source(choice: SourceChoice) -> Result<(&'static str, SourceFactory), GamepadError> {
    let unavailable = |reason: &str| GamepadError::Unavailable {
        choice,
        reason: reason.to_owned(),
    };
    match choice {
        SourceChoice::Null => Ok((
            "null",
            Arc::new(|| -> Box<dyn GamepadSource> { Box::new(NullSource) }),
        )),

        #[cfg(target_os = "linux")]
        SourceChoice::Auto | SourceChoice::Evdev => {
            if choice == SourceChoice::Evdev {
                evdev::probe().map_err(|e| unavailable(&e))?;
            }
            Ok((
                "evdev",
                Arc::new(|| -> Box<dyn GamepadSource> { Box::new(evdev::EvdevSource::new()) }),
            ))
        }
        #[cfg(not(target_os = "linux"))]
        SourceChoice::Evdev => Err(unavailable(
            "evdev is Linux-only; use \"xinput\" on Windows or \"auto\"",
        )),

        #[cfg(windows)]
        SourceChoice::Auto | SourceChoice::XInput => Ok((
            "xinput",
            Arc::new(|| -> Box<dyn GamepadSource> { Box::new(xinput::XInputSource::new()) }),
        )),
        #[cfg(not(windows))]
        SourceChoice::XInput => Err(unavailable(
            "XInput is Windows-only; use \"evdev\" on Linux or \"auto\"",
        )),

        #[cfg(not(any(target_os = "linux", windows)))]
        SourceChoice::Auto => Ok((
            "null",
            Arc::new(|| -> Box<dyn GamepadSource> { Box::new(NullSource) }),
        )),
    }
}

// ------------------------------------------------------------------- pump

/// The host thread that turns a [`GamepadSource`] into guest input.
///
/// Owned by the [`crate::InputDevice`] it feeds and tied to one activation:
/// started when the driver sets `DRIVER_OK`, stopped by a reset or by dropping
/// the device. That is the same lifetime rule virtio-net's receive worker has,
/// and for the same reason — it is a host thread that writes guest memory, so
/// it owes the pause gate a [`Quiesce::wait_while_paused`] before every push
/// (ADR-0005).
pub struct GamepadCapture {
    stop: Arc<AtomicBool>,
    /// Kept so [`Self::stop`] can wake a thread parked on the pause gate: a
    /// device reset runs on a *quiesced* VM, and joining a parked thread
    /// without waking it is a deadlock.
    quiesce: Arc<Quiesce>,
    stats: Arc<CaptureStats>,
    thread: Option<JoinHandle<()>>,
}

impl std::fmt::Debug for GamepadCapture {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("GamepadCapture")
            .field("running", &self.thread.is_some())
            .field("connected", &self.stats.connected.load(Ordering::Relaxed))
            .finish_non_exhaustive()
    }
}

/// Counters the pump keeps, for `entangled doctor` and the tests.
#[derive(Debug, Default)]
pub struct CaptureStats {
    /// Reports (one delta batch) pushed to the guest.
    pub reports: AtomicU64,
    /// Individual events pushed, `SYN_REPORT`s included.
    pub events: AtomicU64,
    /// How many times a controller appeared (a plug-in, or a swap).
    pub connects: AtomicU64,
    /// How many times one disappeared.
    pub disconnects: AtomicU64,
    /// 1 while a controller is attached, 0 otherwise. An `AtomicU64` rather
    /// than a bool so the whole struct is one shape.
    pub connected: AtomicU64,
}

impl GamepadCapture {
    /// Starts the pump. `sink` is the device's own [`InputHandle`], so events
    /// keep working across a guest's own resets of the ring.
    pub fn start(
        sink: InputHandle,
        quiesce: Arc<Quiesce>,
        mut source: Box<dyn GamepadSource>,
    ) -> Result<Self, GamepadError> {
        let stop = Arc::new(AtomicBool::new(false));
        let stats = Arc::new(CaptureStats::default());
        let context = (
            Arc::clone(&stop),
            Arc::clone(&quiesce),
            Arc::clone(&stats),
            sink,
        );
        let thread = std::thread::Builder::new()
            .name("entangled-gamepad".to_owned())
            .spawn(move || {
                let (stop, quiesce, stats, sink) = context;
                pump_loop(source.as_mut(), &sink, &quiesce, &stop, &stats);
            })
            .map_err(|e| GamepadError::Spawn(e.to_string()))?;
        Ok(Self {
            stop,
            quiesce,
            stats,
            thread: Some(thread),
        })
    }

    /// Live counters.
    pub fn stats(&self) -> &Arc<CaptureStats> {
        &self.stats
    }

    /// Stops and joins the pump. Idempotent and infallible, so both `reset()`
    /// and `Drop` can call it.
    pub fn stop(&mut self) {
        let Some(thread) = self.thread.take() else {
            return;
        };
        self.stop.store(true, Ordering::Release);
        // The thread may be parked on the pause gate; a reset happens while the
        // VM is quiesced, so without this the join below never returns.
        self.quiesce.wake();
        match thread.join() {
            Ok(()) => tracing::debug!("gamepad capture stopped"),
            // A panicking host thread is a host bug, never guest-triggered.
            Err(_) => tracing::error!("gamepad capture thread panicked"),
        }
    }
}

impl Drop for GamepadCapture {
    fn drop(&mut self) {
        self.stop();
    }
}

/// Poll, diff, push — until asked to stop.
fn pump_loop(
    source: &mut dyn GamepadSource,
    sink: &InputHandle,
    quiesce: &Quiesce,
    stop: &AtomicBool,
    stats: &CaptureStats,
) {
    let mut last = PadState::NEUTRAL;
    let mut last_id: Option<PadId> = None;

    while !stop.load(Ordering::Acquire) {
        // Nothing below this line may touch guest memory while the VM is
        // paused (ADR-0005). Parked *before* the poll rather than after it, so
        // a resumed VM sees the pad's state now and not its state a pause ago.
        let Some(_pass) = quiesce.wait_while_paused(|| !stop.load(Ordering::Acquire)) else {
            return;
        };

        let target = match source.poll(POLL_INTERVAL) {
            Poll::Connected { id, state } => {
                if last_id.as_ref() != Some(&id) {
                    stats.connects.fetch_add(1, Ordering::Relaxed);
                    stats.connected.store(1, Ordering::Relaxed);
                    tracing::info!(
                        source = source.name(),
                        controller = %id.label,
                        slot = id.slot,
                        "gamepad connected"
                    );
                    last_id = Some(id);
                }
                state
            }
            Poll::Disconnected => {
                if let Some(previous) = last_id.take() {
                    stats.disconnects.fetch_add(1, Ordering::Relaxed);
                    stats.connected.store(0, Ordering::Relaxed);
                    tracing::info!(
                        source = source.name(),
                        controller = %previous.label,
                        "gamepad disconnected; releasing everything it was holding"
                    );
                }
                // The whole unplug story: neutral is a state like any other, so
                // the diff below releases held buttons and re-centres the
                // sticks — and every later tick produces nothing at all.
                PadState::NEUTRAL
            }
        };

        let events = target.delta(&last);
        if events.is_empty() {
            continue;
        }
        debug_assert!(events.len() <= MAX_EVENTS_PER_REPORT);
        match sink.push(&events) {
            Ok(_) => {
                stats.reports.fetch_add(1, Ordering::Relaxed);
                stats
                    .events
                    .fetch_add(events.len() as u64, Ordering::Relaxed);
                // Only advance once the events are the guest's problem. If the
                // push failed the next tick recomputes the same delta, so a
                // transient host error costs latency and not correctness.
                last = target;
            }
            Err(error) => {
                tracing::warn!(%error, "gamepad event delivery failed");
            }
        }
    }
}

/// Clamps and rescales a host axis reading onto a guest axis range.
///
/// In `i64` throughout: a host pad may report `-32768..=32767` and the
/// intermediate product overflows `i32` well before the ends of that range.
/// A degenerate source range (min >= max) maps to the middle of the
/// destination rather than dividing by zero — a pad whose `ABS_INFO` says an
/// axis has no travel is broken, not fatal.
pub fn rescale(value: i32, src: (i32, i32), dst: (i32, i32)) -> i32 {
    let (src_min, src_max) = (i64::from(src.0), i64::from(src.1));
    let (dst_min, dst_max) = (i64::from(dst.0), i64::from(dst.1));
    if src_min >= src_max {
        return ((dst_min + dst_max) / 2) as i32;
    }
    let value = i64::from(value).clamp(src_min, src_max);
    let span = src_max - src_min;
    let scaled = dst_min + (value - src_min) * (dst_max - dst_min) / span;
    scaled.clamp(dst_min, dst_max) as i32
}

/// Rescales onto the stick range and saturates into `i16`.
pub fn to_stick(value: i32, src: (i32, i32)) -> i16 {
    let scaled = rescale(value, src, (pad::STICK_MIN, pad::STICK_MAX));
    scaled.clamp(i32::from(i16::MIN), i32::from(i16::MAX)) as i16
}

/// Rescales onto the trigger range and saturates into `u8`.
pub fn to_trigger(value: i32, src: (i32, i32)) -> u8 {
    rescale(value, src, (pad::TRIGGER_MIN, pad::TRIGGER_MAX)).clamp(0, 255) as u8
}

/// Turns a host hat reading into the guest's `-1..=1`, whatever range the host
/// reported it in. A hat is a sign, not a magnitude.
pub fn to_hat(value: i32) -> i8 {
    match value.signum() {
        n if n < 0 => -1,
        0 => 0,
        _ => 1,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::Profile;

    fn pressed(codes: &[u16]) -> PadState {
        let mut state = PadState::NEUTRAL;
        for &code in codes {
            assert!(state.set_button_by_code(code, true), "{code:#x}");
        }
        state
    }

    #[test]
    fn a_neutral_pad_produces_no_events_at_all() {
        assert!(PadState::NEUTRAL.delta(&PadState::NEUTRAL).is_empty());
        assert_eq!(PadState::default(), PadState::NEUTRAL);
    }

    #[test]
    fn one_button_press_is_one_event_plus_a_syn() {
        let events = pressed(&[btn::SOUTH]).delta(&PadState::NEUTRAL);
        assert_eq!(
            events,
            vec![
                InputEvent {
                    event_type: ev::KEY,
                    code: btn::SOUTH,
                    value: 1
                },
                InputEvent::SYN_REPORT,
            ]
        );
        // …and releasing it again is the mirror image.
        let events = PadState::NEUTRAL.delta(&pressed(&[btn::SOUTH]));
        assert_eq!(events[0].value, 0);
        assert_eq!(events.len(), 2);
    }

    #[test]
    fn axes_are_reported_as_twos_complement_and_only_when_they_move() {
        let mut state = PadState::NEUTRAL;
        state.left_stick = (-32768, 32767);
        state.right_trigger = 255;
        state.hat = (0, -1);
        let events = state.delta(&PadState::NEUTRAL);
        assert_eq!(
            events,
            vec![
                InputEvent {
                    event_type: ev::ABS,
                    code: abs::X,
                    value: (-32768i32) as u32
                },
                InputEvent {
                    event_type: ev::ABS,
                    code: abs::Y,
                    value: 32767
                },
                InputEvent {
                    event_type: ev::ABS,
                    code: abs::RZ,
                    value: 255
                },
                InputEvent {
                    event_type: ev::ABS,
                    code: abs::HAT0Y,
                    value: (-1i32) as u32
                },
                InputEvent::SYN_REPORT,
            ]
        );
        // The wire form of ABS_X = -32768 is what the guest reads as __s32.
        assert_eq!(
            events[0].to_le_bytes(),
            [0x03, 0x00, 0x00, 0x00, 0x00, 0x80, 0xff, 0xff]
        );
        // Nothing moved since: nothing is sent.
        assert!(state.delta(&state).is_empty());
    }

    #[test]
    fn every_event_the_pump_can_produce_is_one_the_profile_advertises() {
        // The whole point of a bounded, enumerated state: there is no path
        // from a host controller to an event the guest device never claimed.
        let mut everything = PadState {
            buttons: [true; BUTTON_COUNT],
            left_stick: (-1, 1),
            right_stick: (2, -2),
            left_trigger: 1,
            right_trigger: 2,
            hat: (1, -1),
        };
        let events = everything.delta(&PadState::NEUTRAL);
        assert_eq!(events.len(), MAX_EVENTS_PER_REPORT);
        for event in &events {
            assert!(
                Profile::Gamepad.accepts(*event),
                "the gamepad profile must advertise {event:?}"
            );
        }
        // …and the same in reverse.
        everything.buttons = [false; BUTTON_COUNT];
        for event in PadState::NEUTRAL.delta(&everything) {
            assert!(Profile::Gamepad.accepts(event));
        }
    }

    #[test]
    fn unknown_button_codes_are_refused_rather_than_indexed() {
        let mut state = PadState::NEUTRAL;
        for code in [0u16, 0x110, 0x132, 0x13f, 0x220, u16::MAX] {
            assert!(!state.set_button_by_code(code, true));
            assert!(!state.button_by_code(code));
        }
        assert_eq!(state, PadState::NEUTRAL);
        for code in btn::GAMEPAD {
            assert!(state.set_button_by_code(code, true));
            assert!(state.button_by_code(code));
        }
        assert_eq!(state.buttons, [true; BUTTON_COUNT]);
    }

    #[test]
    fn rescaling_hits_both_ends_and_the_middle_and_never_divides_by_zero() {
        assert_eq!(to_stick(0, (0, 255)), pad::STICK_MIN as i16);
        assert_eq!(to_stick(255, (0, 255)), pad::STICK_MAX as i16);
        assert_eq!(to_stick(-32768, (-32768, 32767)), -32768);
        assert_eq!(to_stick(32767, (-32768, 32767)), 32767);
        assert_eq!(to_stick(0, (-32768, 32767)), 0);
        // Out of the source range clamps rather than wrapping.
        assert_eq!(to_stick(9999, (0, 255)), pad::STICK_MAX as i16);
        assert_eq!(to_stick(-9999, (0, 255)), pad::STICK_MIN as i16);
        // A pad claiming an axis with no travel must not divide by zero.
        assert_eq!(to_stick(7, (5, 5)), 0); // midpoint of -32768..=32767
        assert_eq!(to_trigger(7, (0, 0)), 127);

        assert_eq!(to_trigger(0, (0, 1023)), 0);
        assert_eq!(to_trigger(1023, (0, 1023)), 255);
        assert_eq!(to_trigger(512, (0, 1023)), 127);
        // The identity case an XInput host takes.
        for raw in [0, 1, 128, 254, 255] {
            assert_eq!(to_trigger(raw, (0, 255)), raw as u8);
        }

        assert_eq!((to_hat(-7), to_hat(0), to_hat(7)), (-1, 0, 1));
        assert_eq!((to_hat(i32::MIN), to_hat(i32::MAX)), (-1, 1));
    }

    #[test]
    fn the_null_source_is_never_connected_and_paces_itself() {
        let mut source = NullSource;
        assert_eq!(source.name(), "null");
        let started = std::time::Instant::now();
        assert_eq!(source.poll(Duration::from_millis(5)), Poll::Disconnected);
        assert!(started.elapsed() >= Duration::from_millis(4));
    }

    #[test]
    fn open_source_never_fails_on_auto_and_refuses_a_foreign_mechanism() {
        let (name, factory) = open_source(SourceChoice::Auto).expect("auto never fails");
        assert!(["evdev", "xinput", "null"].contains(&name));
        // The factory really makes a working source, twice.
        assert_eq!(factory().name(), name);
        assert_eq!(factory().name(), name);

        let (name, _) = open_source(SourceChoice::Null).expect("null is everywhere");
        assert_eq!(name, "null");

        #[cfg(not(target_os = "linux"))]
        assert!(open_source(SourceChoice::Evdev).is_err());
        #[cfg(not(windows))]
        assert!(open_source(SourceChoice::XInput).is_err());
        #[cfg(windows)]
        assert_eq!(
            open_source(SourceChoice::XInput).map(|(n, _)| n).ok(),
            Some("xinput")
        );

        assert_eq!(SourceChoice::default(), SourceChoice::Auto);
        assert_eq!(SourceChoice::Auto.to_string(), "auto");
        assert_eq!(SourceChoice::XInput.to_string(), "xinput");
    }

    // ------------------------------------------------------------- the pump

    /// A source the test drives: a script of polls, then disconnected for ever.
    struct ScriptedSource {
        script: std::sync::Mutex<std::collections::VecDeque<Poll>>,
    }

    impl ScriptedSource {
        fn new(script: Vec<Poll>) -> Self {
            Self {
                script: std::sync::Mutex::new(script.into()),
            }
        }
    }

    impl GamepadSource for ScriptedSource {
        fn name(&self) -> &'static str {
            "scripted"
        }
        fn poll(&mut self, timeout: Duration) -> Poll {
            let next = self
                .script
                .lock()
                .map(|mut q| q.pop_front())
                .unwrap_or(None);
            match next {
                Some(poll) => poll,
                None => {
                    std::thread::sleep(timeout);
                    Poll::Disconnected
                }
            }
        }
    }

    fn id(label: &str, slot: u64) -> PadId {
        PadId {
            label: label.to_owned(),
            slot,
        }
    }

    /// The pump against a real device, with a real guest ring behind it, is in
    /// `tests/gamepad_queue.rs`; here we only need the sink to exist.
    fn sink_of(device: &crate::InputDevice) -> InputHandle {
        device.handle()
    }

    #[test]
    fn the_pump_stops_and_joins_even_while_parked_on_the_pause_gate() {
        let device = crate::InputDevice::gamepad();
        let quiesce = Quiesce::new();
        quiesce.pause();
        let mut capture =
            GamepadCapture::start(sink_of(&device), Arc::clone(&quiesce), Box::new(NullSource))
                .expect("the pump starts");
        // Parked: a stop must still return, which is the deadlock virtio-net's
        // `stop_rx` documents.
        std::thread::sleep(Duration::from_millis(20));
        capture.stop();
        capture.stop(); // idempotent
        assert_eq!(capture.stats().reports.load(Ordering::Relaxed), 0);
    }

    #[test]
    fn an_unplug_releases_what_the_pad_was_holding_and_then_goes_quiet() {
        let device = crate::InputDevice::gamepad();
        let sink = device.handle();
        let quiesce = Quiesce::new();
        let mut held = PadState::NEUTRAL;
        assert!(held.set_button_by_code(btn::SOUTH, true));
        held.left_stick = (-32768, 0);
        let source = ScriptedSource::new(vec![
            Poll::Connected {
                id: id("pad", 0),
                state: held,
            },
            Poll::Disconnected,
        ]);
        let mut capture =
            GamepadCapture::start(sink, Arc::clone(&quiesce), Box::new(source)).expect("started");
        // The device is not activated, so nothing reaches a ring — but the pump
        // still runs the whole diff, which is what these counters measure.
        std::thread::sleep(Duration::from_millis(120));
        capture.stop();

        let stats = capture.stats();
        assert_eq!(stats.connects.load(Ordering::Relaxed), 1);
        assert_eq!(stats.disconnects.load(Ordering::Relaxed), 1);
        assert_eq!(stats.connected.load(Ordering::Relaxed), 0);
        // Exactly two reports: press+move, then release+centre. Never more,
        // however long the pad stays unplugged.
        assert_eq!(
            stats.reports.load(Ordering::Relaxed),
            2,
            "an absent controller must not produce a report per tick"
        );
        assert_eq!(stats.events.load(Ordering::Relaxed), 6);
    }

    #[test]
    fn swapping_controllers_is_a_diff_not_a_reset() {
        let device = crate::InputDevice::gamepad();
        let quiesce = Quiesce::new();
        let mut a = PadState::NEUTRAL;
        assert!(a.set_button_by_code(btn::SOUTH, true));
        let mut b = PadState::NEUTRAL;
        assert!(b.set_button_by_code(btn::SOUTH, true));
        assert!(b.set_button_by_code(btn::START, true));
        let source = ScriptedSource::new(vec![
            Poll::Connected {
                id: id("pad A", 0),
                state: a,
            },
            Poll::Connected {
                id: id("pad B", 1),
                state: b,
            },
        ]);
        let mut capture =
            GamepadCapture::start(device.handle(), quiesce, Box::new(source)).expect("started");
        std::thread::sleep(Duration::from_millis(120));
        capture.stop();
        let stats = capture.stats();
        assert_eq!(stats.connects.load(Ordering::Relaxed), 2);
        // Three reports: A's press, B's extra press, then the neutralising
        // release when the script runs out. BTN_SOUTH is held across the swap
        // and is *not* re-sent, because both pads have it down.
        assert_eq!(stats.reports.load(Ordering::Relaxed), 3);
        assert_eq!(stats.events.load(Ordering::Relaxed), 2 + 2 + 3);
    }
}
