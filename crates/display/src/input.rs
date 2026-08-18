//! Host input capture: winit events → `virtio-input` event batches
//! (backlog MVP-902/904/905/906/907, host half of EPIC 9).
//!
//! The host never interprets keys, it only translates *positions*: winit
//! [`PhysicalKey::Code`] goes through [`crate::keymap`] to a Linux `KEY_*` code
//! and straight into the guest. Every group of events is terminated with
//! [`InputEvent::SYN_REPORT`] (MVP-905) and queued for the `virtio-input`
//! device, which drains it from its own thread.
//!
//! Two shortcuts are consumed by the host and never reach the guest (MVP-907):
//!
//! | Shortcut | Effect |
//! |---|---|
//! | `Ctrl+Alt+G` | toggle pointer grab ([`ControlEvent::GrabToggled`]) |
//! | `Ctrl+Alt+Q` | ask the VM to shut down ([`ControlEvent::QuitRequested`]) |

use std::collections::{BTreeSet, VecDeque};
use std::sync::{Arc, Mutex};

use virtio_input::{abs, btn, ev, InputEvent};
use winit::event::{ElementState, MouseButton, MouseScrollDelta};
use winit::keyboard::{KeyCode, PhysicalKey};

use crate::keymap::linux_keycode;
use crate::sync::lock;
use crate::Viewport;

/// Relative axes (`REL_*`), needed for the scroll wheel.
///
/// TODO(virtio-input): upstream these into `virtio_input::rel` — that crate is
/// owned elsewhere, so the host keeps a local copy for now.
pub mod rel {
    /// `REL_HWHEEL` — horizontal scroll, in notches.
    pub const HWHEEL: u16 = 0x06;
    /// `REL_WHEEL` — vertical scroll, in notches.
    pub const WHEEL: u16 = 0x08;
}

/// Extra mouse buttons beyond `virtio_input::btn`.
///
/// TODO(virtio-input): upstream into `virtio_input::btn`.
pub mod btn_ext {
    /// `BTN_SIDE`.
    pub const SIDE: u16 = 0x113;
    /// `BTN_EXTRA`.
    pub const EXTRA: u16 = 0x114;
}

/// Linux `KEY_*` codes of the modifiers the reserved shortcuts need.
const KEY_LEFTCTRL: u16 = 29;
const KEY_RIGHTCTRL: u16 = 97;
const KEY_LEFTALT: u16 = 56;
const KEY_RIGHTALT: u16 = 100;

/// `EV_KEY` values.
const KEY_UP: u32 = 0;
const KEY_DOWN: u32 = 1;

/// How many un-drained batches the host keeps before dropping the oldest.
///
/// A guest that stops servicing its input queues must not grow host memory
/// without bound; ~256 batches is far more than any human can type inside one
/// stalled frame.
pub const MAX_PENDING_BATCHES: usize = 256;

/// How many pending control events are kept (grab toggles, quit requests).
pub const MAX_PENDING_CONTROL: usize = 64;

/// Host-consumed events the VM supervisor acts on (backlog MVP-907).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ControlEvent {
    /// `Ctrl+Alt+G`: the new grab state after toggling.
    GrabToggled(bool),
    /// `Ctrl+Alt+Q`: the user asked the VM to shut down.
    QuitRequested,
    /// The window's close button was used.
    WindowCloseRequested,
}

/// What happened to one key press/release.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum KeyOutcome {
    /// Forwarded to the guest as this Linux `KEY_*` code.
    Forwarded(u16),
    /// Swallowed by the host as a reserved shortcut.
    Reserved(ControlEvent),
    /// Dropped: no evdev equivalent, a key repeat, or a release of a key the
    /// guest never saw pressed.
    Ignored,
}

/// Counters for the periodic diagnostics event (MVP-708).
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct InputStats {
    /// Event batches queued for the guest.
    pub batches: u64,
    /// Individual events queued (including `SYN_REPORT`).
    pub events: u64,
    /// Batches dropped because the guest was not draining.
    pub dropped: u64,
}

#[derive(Debug, Default)]
struct Ring {
    batches: VecDeque<Vec<InputEvent>>,
    stats: InputStats,
}

/// The captured input stream, shared between the event loop (producer) and the
/// `virtio-input` device (consumer).
///
/// Cloning gives another handle to the same queue. Batches are kept whole so a
/// consumer never sees events without their terminating `SYN_REPORT`.
#[derive(Debug, Clone, Default)]
pub struct InputQueue {
    inner: Arc<Mutex<Ring>>,
}

impl InputQueue {
    /// Creates an empty queue.
    pub fn new() -> Self {
        Self::default()
    }

    /// Queues one complete batch. Called by [`InputCapture`] only.
    fn push(&self, batch: Vec<InputEvent>) {
        if batch.is_empty() {
            return;
        }
        let mut ring = lock(&self.inner, "input queue");
        if ring.batches.len() >= MAX_PENDING_BATCHES {
            let _ = ring.batches.pop_front();
            ring.stats.dropped += 1;
            tracing::warn!(
                dropped = ring.stats.dropped,
                "guest is not draining input; dropped the oldest batch"
            );
        }
        ring.stats.batches += 1;
        ring.stats.events += batch.len() as u64;
        ring.batches.push_back(batch);
    }

    /// Takes every pending event as one flat, `SYN_REPORT`-delimited stream —
    /// what a `virtio-input` event queue wants.
    pub fn drain(&self) -> Vec<InputEvent> {
        self.drain_batches().into_iter().flatten().collect()
    }

    /// Takes every pending batch, preserving batch boundaries.
    pub fn drain_batches(&self) -> Vec<Vec<InputEvent>> {
        let mut ring = lock(&self.inner, "input queue");
        ring.batches.drain(..).collect()
    }

    /// True when nothing is pending.
    pub fn is_empty(&self) -> bool {
        lock(&self.inner, "input queue").batches.is_empty()
    }

    /// Counters since start.
    pub fn stats(&self) -> InputStats {
        lock(&self.inner, "input queue").stats
    }
}

/// Pending host control events (grab toggles, quit requests).
#[derive(Debug, Clone, Default)]
pub struct ControlQueue {
    inner: Arc<Mutex<VecDeque<ControlEvent>>>,
}

impl ControlQueue {
    /// Creates an empty queue.
    pub fn new() -> Self {
        Self::default()
    }

    /// Queues a control event, dropping the oldest if nobody is listening.
    pub fn push(&self, event: ControlEvent) {
        let mut queue = lock(&self.inner, "control queue");
        if queue.len() >= MAX_PENDING_CONTROL {
            let _ = queue.pop_front();
        }
        queue.push_back(event);
    }

    /// Takes every pending control event.
    pub fn drain(&self) -> Vec<ControlEvent> {
        lock(&self.inner, "control queue").drain(..).collect()
    }
}

/// Accumulates fractional scroll deltas into whole wheel notches.
///
/// Touchpads and Wayland send fractional lines or raw pixels; the guest expects
/// integral `REL_WHEEL` notches, so the remainder is carried to the next event
/// instead of being rounded away.
#[derive(Debug, Clone, Copy, Default)]
pub struct WheelAccumulator {
    notches: f64,
}

impl WheelAccumulator {
    /// Pixels per wheel notch for [`MouseScrollDelta::PixelDelta`]. Chosen to
    /// feel like one notch per "line" of a typical toolkit (Wayland reports
    /// roughly 10–15 px per detent, X11 sends lines instead).
    pub const PIXELS_PER_NOTCH: f64 = 40.0;

    /// Adds a line delta, returning the whole notches to emit now.
    pub fn add_lines(&mut self, lines: f64) -> i32 {
        self.add(lines)
    }

    /// Adds a pixel delta, returning the whole notches to emit now.
    pub fn add_pixels(&mut self, pixels: f64) -> i32 {
        self.add(pixels / Self::PIXELS_PER_NOTCH)
    }

    fn add(&mut self, delta: f64) -> i32 {
        if !delta.is_finite() {
            return 0;
        }
        self.notches = (self.notches + delta).clamp(-1024.0, 1024.0);
        let whole = self.notches.trunc();
        self.notches -= whole;
        whole as i32
    }
}

/// Maps a physical window position to `ABS_X`/`ABS_Y` events against the
/// *viewport*, so the guest cursor sits exactly under the host cursor over the
/// image and clamps to the edge over the letterbox bars (MVP-903).
pub fn pointer_abs_events(viewport: &Viewport, x: f64, y: f64) -> [InputEvent; 2] {
    let (local_x, local_y) = viewport.to_local(x, y);
    [
        InputEvent::abs_from_window(abs::X, local_x, f64::from(viewport.width)),
        InputEvent::abs_from_window(abs::Y, local_y, f64::from(viewport.height)),
    ]
}

/// Translates winit input events into guest input batches.
///
/// Free of window and GPU state, so the whole thing is unit-tested headlessly;
/// the event loop only feeds it and reacts to [`KeyOutcome`].
#[derive(Debug)]
pub struct InputCapture {
    events: InputQueue,
    control: ControlQueue,
    /// `EV_KEY` codes currently down — keyboard keys *and* mouse buttons, which
    /// share the `EV_KEY` code space in Linux.
    pressed: BTreeSet<u16>,
    grabbed: bool,
    focused: bool,
    vertical: WheelAccumulator,
    horizontal: WheelAccumulator,
    unmapped: u64,
}

impl InputCapture {
    /// Wires a capture to the queues the devices drain.
    pub fn new(events: InputQueue, control: ControlQueue) -> Self {
        Self {
            events,
            control,
            pressed: BTreeSet::new(),
            grabbed: false,
            focused: false,
            vertical: WheelAccumulator::default(),
            horizontal: WheelAccumulator::default(),
            unmapped: 0,
        }
    }

    /// The guest-facing event queue.
    pub fn events(&self) -> InputQueue {
        self.events.clone()
    }

    /// The host-facing control queue.
    pub fn control(&self) -> ControlQueue {
        self.control.clone()
    }

    /// Whether the pointer is currently grabbed (`Ctrl+Alt+G`).
    pub fn grabbed(&self) -> bool {
        self.grabbed
    }

    /// Whether the window currently has keyboard focus.
    pub fn focused(&self) -> bool {
        self.focused
    }

    /// Number of keys/buttons the guest currently believes are held.
    pub fn pressed_count(&self) -> usize {
        self.pressed.len()
    }

    /// Keys seen that have no Linux equivalent (diagnostics).
    pub fn unmapped_keys(&self) -> u64 {
        self.unmapped
    }

    /// Handles one keyboard event. `repeat` is winit's auto-repeat flag: the
    /// guest's input core generates its own repeats, so host repeats are
    /// dropped.
    pub fn on_key(&mut self, key: PhysicalKey, state: ElementState, repeat: bool) -> KeyOutcome {
        let PhysicalKey::Code(code) = key else {
            self.unmapped += 1;
            tracing::trace!(?key, "unidentified physical key dropped");
            return KeyOutcome::Ignored;
        };
        if repeat {
            return KeyOutcome::Ignored;
        }
        if let Some(event) = self.reserved_shortcut(code, state) {
            // The modifiers themselves were already forwarded, so release
            // everything the guest thinks is held — otherwise it keeps a stuck
            // Ctrl+Alt after the shortcut.
            self.release_all();
            self.control.push(event);
            tracing::debug!(?event, "reserved shortcut consumed by the host");
            return KeyOutcome::Reserved(event);
        }
        let Some(linux) = linux_keycode(code) else {
            self.unmapped += 1;
            tracing::trace!(?code, "no Linux keycode for this physical key");
            return KeyOutcome::Ignored;
        };
        match state {
            ElementState::Pressed => {
                self.pressed.insert(linux);
                self.emit_key(linux, KEY_DOWN);
            }
            ElementState::Released => {
                if !self.pressed.remove(&linux) {
                    // A release without a matching press (focus was lost while
                    // held) — the guest already got the key-up.
                    return KeyOutcome::Ignored;
                }
                self.emit_key(linux, KEY_UP);
            }
        }
        KeyOutcome::Forwarded(linux)
    }

    /// Handles a pointer move. `viewport` is `None` while the window is
    /// minimized, in which case the motion is dropped.
    pub fn on_pointer(&mut self, viewport: Option<Viewport>, x: f64, y: f64) -> bool {
        let Some(viewport) = viewport else {
            return false;
        };
        if !x.is_finite() || !y.is_finite() {
            return false;
        }
        let [abs_x, abs_y] = pointer_abs_events(&viewport, x, y);
        self.push(vec![abs_x, abs_y]);
        true
    }

    /// Handles a mouse button. Unknown buttons are dropped.
    pub fn on_button(&mut self, button: MouseButton, state: ElementState) -> KeyOutcome {
        let code = match button {
            MouseButton::Left => btn::LEFT,
            MouseButton::Right => btn::RIGHT,
            MouseButton::Middle => btn::MIDDLE,
            MouseButton::Back => btn_ext::SIDE,
            MouseButton::Forward => btn_ext::EXTRA,
            MouseButton::Other(_) => {
                tracing::trace!(?button, "unmapped mouse button dropped");
                return KeyOutcome::Ignored;
            }
        };
        match state {
            ElementState::Pressed => {
                self.pressed.insert(code);
                self.emit_key(code, KEY_DOWN);
            }
            ElementState::Released => {
                if !self.pressed.remove(&code) {
                    return KeyOutcome::Ignored;
                }
                self.emit_key(code, KEY_UP);
            }
        }
        KeyOutcome::Forwarded(code)
    }

    /// Handles a scroll event, emitting whole `REL_WHEEL`/`REL_HWHEEL` notches.
    pub fn on_wheel(&mut self, delta: MouseScrollDelta) {
        let (horizontal, vertical) = match delta {
            MouseScrollDelta::LineDelta(x, y) => (
                self.horizontal.add_lines(f64::from(x)),
                self.vertical.add_lines(f64::from(y)),
            ),
            MouseScrollDelta::PixelDelta(pos) => (
                self.horizontal.add_pixels(pos.x),
                self.vertical.add_pixels(pos.y),
            ),
        };
        let mut batch = Vec::new();
        if vertical != 0 {
            batch.push(InputEvent {
                event_type: ev::REL,
                code: rel::WHEEL,
                value: vertical as u32,
            });
        }
        if horizontal != 0 {
            batch.push(InputEvent {
                event_type: ev::REL,
                code: rel::HWHEEL,
                value: horizontal as u32,
            });
        }
        self.push(batch);
    }

    /// Handles focus changes. Losing focus releases every held key and button
    /// so the guest cannot end up with a stuck modifier (MVP-906).
    pub fn on_focus(&mut self, focused: bool) {
        self.focused = focused;
        if !focused {
            let released = self.pressed.len();
            self.release_all();
            self.grabbed = false;
            if released > 0 {
                tracing::debug!(released, "focus lost: released all held keys");
            }
        }
    }

    /// Emits key-up for everything the guest believes is held, in one batch.
    pub fn release_all(&mut self) {
        if self.pressed.is_empty() {
            return;
        }
        let mut batch = Vec::with_capacity(self.pressed.len() + 1);
        for code in std::mem::take(&mut self.pressed) {
            batch.push(InputEvent {
                event_type: ev::KEY,
                code,
                value: KEY_UP,
            });
        }
        self.push(batch);
        self.vertical = WheelAccumulator::default();
        self.horizontal = WheelAccumulator::default();
    }

    /// `Ctrl+Alt+G` / `Ctrl+Alt+Q` on key *press*, using our own pressed set as
    /// the modifier state — winit's `ModifiersChanged` can go stale across focus
    /// changes, our set cannot (focus loss clears it).
    fn reserved_shortcut(&mut self, code: KeyCode, state: ElementState) -> Option<ControlEvent> {
        if state != ElementState::Pressed || !self.ctrl_down() || !self.alt_down() {
            return None;
        }
        match code {
            KeyCode::KeyG => {
                self.grabbed = !self.grabbed;
                Some(ControlEvent::GrabToggled(self.grabbed))
            }
            KeyCode::KeyQ => Some(ControlEvent::QuitRequested),
            _ => None,
        }
    }

    fn ctrl_down(&self) -> bool {
        self.pressed.contains(&KEY_LEFTCTRL) || self.pressed.contains(&KEY_RIGHTCTRL)
    }

    fn alt_down(&self) -> bool {
        self.pressed.contains(&KEY_LEFTALT) || self.pressed.contains(&KEY_RIGHTALT)
    }

    fn emit_key(&mut self, code: u16, value: u32) {
        self.push(vec![InputEvent {
            event_type: ev::KEY,
            code,
            value,
        }]);
    }

    /// Terminates a batch with `SYN_REPORT` (MVP-905) and queues it.
    fn push(&mut self, mut batch: Vec<InputEvent>) {
        if batch.is_empty() {
            return;
        }
        batch.push(InputEvent::SYN_REPORT);
        self.events.push(batch);
    }
}

#[cfg(test)]
mod tests {
    use winit::dpi::PhysicalPosition;

    use super::*;

    fn capture() -> InputCapture {
        InputCapture::new(InputQueue::new(), ControlQueue::new())
    }

    fn key(code: KeyCode) -> PhysicalKey {
        PhysicalKey::Code(code)
    }

    fn press(c: &mut InputCapture, code: KeyCode) -> KeyOutcome {
        c.on_key(key(code), ElementState::Pressed, false)
    }

    fn release(c: &mut InputCapture, code: KeyCode) -> KeyOutcome {
        c.on_key(key(code), ElementState::Released, false)
    }

    #[test]
    fn key_press_and_release_are_syn_terminated() {
        let mut c = capture();
        assert_eq!(press(&mut c, KeyCode::KeyA), KeyOutcome::Forwarded(30));
        assert_eq!(release(&mut c, KeyCode::KeyA), KeyOutcome::Forwarded(30));
        let batches = c.events().drain_batches();
        assert_eq!(batches.len(), 2);
        assert_eq!(
            batches[0],
            vec![
                InputEvent {
                    event_type: ev::KEY,
                    code: 30,
                    value: 1
                },
                InputEvent::SYN_REPORT
            ]
        );
        assert_eq!(batches[1][0].value, 0);
        assert_eq!(*batches[1].last().unwrap(), InputEvent::SYN_REPORT);
        assert_eq!(c.pressed_count(), 0);
    }

    #[test]
    fn repeats_and_unmapped_keys_are_dropped() {
        let mut c = capture();
        assert_eq!(
            c.on_key(key(KeyCode::KeyA), ElementState::Pressed, true),
            KeyOutcome::Ignored
        );
        assert_eq!(press(&mut c, KeyCode::Fn), KeyOutcome::Ignored);
        assert_eq!(
            c.on_key(
                PhysicalKey::Unidentified(winit::keyboard::NativeKeyCode::Unidentified),
                ElementState::Pressed,
                false
            ),
            KeyOutcome::Ignored
        );
        assert!(c.events().is_empty());
        assert_eq!(c.unmapped_keys(), 2);
    }

    #[test]
    fn release_without_press_is_dropped() {
        let mut c = capture();
        assert_eq!(release(&mut c, KeyCode::KeyA), KeyOutcome::Ignored);
        assert!(c.events().is_empty());
    }

    #[test]
    fn focus_loss_releases_every_held_key() {
        let mut c = capture();
        press(&mut c, KeyCode::ControlLeft);
        press(&mut c, KeyCode::ShiftLeft);
        press(&mut c, KeyCode::KeyA);
        c.on_button(MouseButton::Left, ElementState::Pressed);
        assert_eq!(c.pressed_count(), 4);
        let _ = c.events().drain();

        c.on_focus(false);
        assert_eq!(c.pressed_count(), 0);
        let batches = c.events().drain_batches();
        assert_eq!(batches.len(), 1, "one batch, one SYN_REPORT");
        let batch = &batches[0];
        assert_eq!(*batch.last().unwrap(), InputEvent::SYN_REPORT);
        let released: Vec<(u16, u32)> = batch
            .iter()
            .filter(|e| e.event_type == ev::KEY)
            .map(|e| (e.code, e.value))
            .collect();
        assert_eq!(
            released,
            vec![(29, 0), (30, 0), (42, 0), (btn::LEFT, 0)],
            "ctrl, a, shift and BTN_LEFT all released"
        );
        // A late release of a key the guest already saw released is dropped.
        assert_eq!(release(&mut c, KeyCode::KeyA), KeyOutcome::Ignored);
        assert!(c.events().is_empty());
    }

    #[test]
    fn focus_loss_drops_the_grab() {
        let mut c = capture();
        press(&mut c, KeyCode::ControlLeft);
        press(&mut c, KeyCode::AltLeft);
        press(&mut c, KeyCode::KeyG);
        assert!(c.grabbed());
        c.on_focus(false);
        assert!(!c.grabbed());
    }

    #[test]
    fn reserved_shortcuts_never_reach_the_guest() {
        let mut c = capture();
        press(&mut c, KeyCode::ControlLeft);
        press(&mut c, KeyCode::AltLeft);
        let _ = c.events().drain();

        assert_eq!(
            press(&mut c, KeyCode::KeyG),
            KeyOutcome::Reserved(ControlEvent::GrabToggled(true))
        );
        assert!(c.grabbed());
        // Ctrl and Alt were released towards the guest, KEY_G (34) never sent.
        let sent: Vec<u16> = c
            .events()
            .drain()
            .iter()
            .filter(|e| e.event_type == ev::KEY)
            .map(|e| e.code)
            .collect();
        assert!(!sent.contains(&34));
        assert_eq!(c.pressed_count(), 0);

        // Hold the modifiers again to toggle back off.
        press(&mut c, KeyCode::ControlRight);
        press(&mut c, KeyCode::AltRight);
        assert_eq!(
            press(&mut c, KeyCode::KeyG),
            KeyOutcome::Reserved(ControlEvent::GrabToggled(false))
        );
        assert!(!c.grabbed());

        press(&mut c, KeyCode::ControlLeft);
        press(&mut c, KeyCode::AltLeft);
        assert_eq!(
            press(&mut c, KeyCode::KeyQ),
            KeyOutcome::Reserved(ControlEvent::QuitRequested)
        );
        assert_eq!(
            c.control().drain(),
            vec![
                ControlEvent::GrabToggled(true),
                ControlEvent::GrabToggled(false),
                ControlEvent::QuitRequested
            ]
        );
    }

    #[test]
    fn g_and_q_reach_the_guest_without_both_modifiers() {
        let mut c = capture();
        assert_eq!(press(&mut c, KeyCode::KeyG), KeyOutcome::Forwarded(34));
        press(&mut c, KeyCode::ControlLeft);
        // Ctrl alone is not the shortcut.
        assert_eq!(press(&mut c, KeyCode::KeyQ), KeyOutcome::Forwarded(16));
        assert!(!c.grabbed());
        assert!(c.control().drain().is_empty());
    }

    #[test]
    fn pointer_maps_into_the_viewport_and_clamps() {
        let mut c = capture();
        let viewport = Viewport {
            x: 320,
            y: 0,
            width: 1280,
            height: 720,
        };
        // Top-left corner of the image.
        assert!(c.on_pointer(Some(viewport), 320.0, 0.0));
        let batch = c.events().drain_batches().remove(0);
        assert_eq!(batch[0].value, 0);
        assert_eq!(batch[1].value, 0);
        assert_eq!(batch[2], InputEvent::SYN_REPORT);

        // Bottom-right corner.
        assert!(c.on_pointer(Some(viewport), 1600.0, 720.0));
        let batch = c.events().drain_batches().remove(0);
        assert_eq!(batch[0].value, virtio_input::ABS_AXIS_MAX);
        assert_eq!(batch[1].value, virtio_input::ABS_AXIS_MAX);

        // On the left letterbox bar: clamped to the left edge, not negative.
        assert!(c.on_pointer(Some(viewport), 0.0, 360.0));
        let batch = c.events().drain_batches().remove(0);
        assert_eq!(batch[0].code, abs::X);
        assert_eq!(batch[0].value, 0);
        assert!(batch[1].value > 0);

        // Past the right edge: clamped to the maximum.
        assert!(c.on_pointer(Some(viewport), 5000.0, 360.0));
        let batch = c.events().drain_batches().remove(0);
        assert_eq!(batch[0].value, virtio_input::ABS_AXIS_MAX);
    }

    #[test]
    fn pointer_is_dropped_while_minimized_or_bogus() {
        let mut c = capture();
        assert!(!c.on_pointer(None, 10.0, 10.0));
        let viewport = Viewport {
            x: 0,
            y: 0,
            width: 100,
            height: 100,
        };
        assert!(!c.on_pointer(Some(viewport), f64::NAN, 1.0));
        assert!(c.events().is_empty());
    }

    #[test]
    fn pointer_center_matches_the_viewport_center() {
        let mut c = capture();
        // A 2x upscaled 800x600 guest inside a 1600x1200 window.
        let viewport = crate::letterbox(800, 600, 1600, 1200).unwrap();
        assert!(c.on_pointer(Some(viewport), 800.0, 600.0));
        let batch = c.events().drain_batches().remove(0);
        let half = virtio_input::ABS_AXIS_MAX / 2;
        assert!(batch[0].value.abs_diff(half) <= 1);
        assert!(batch[1].value.abs_diff(half) <= 1);
    }

    #[test]
    fn buttons_map_to_btn_codes() {
        let mut c = capture();
        for (button, code) in [
            (MouseButton::Left, btn::LEFT),
            (MouseButton::Right, btn::RIGHT),
            (MouseButton::Middle, btn::MIDDLE),
            (MouseButton::Back, btn_ext::SIDE),
            (MouseButton::Forward, btn_ext::EXTRA),
        ] {
            assert_eq!(
                c.on_button(button, ElementState::Pressed),
                KeyOutcome::Forwarded(code)
            );
            assert_eq!(
                c.on_button(button, ElementState::Released),
                KeyOutcome::Forwarded(code)
            );
        }
        assert_eq!(
            c.on_button(MouseButton::Other(9), ElementState::Pressed),
            KeyOutcome::Ignored
        );
        assert_eq!(c.pressed_count(), 0);
    }

    #[test]
    fn wheel_emits_whole_notches_and_carries_the_remainder() {
        let mut c = capture();
        c.on_wheel(MouseScrollDelta::LineDelta(0.0, 1.0));
        let batch = c.events().drain_batches().remove(0);
        assert_eq!(batch[0].event_type, ev::REL);
        assert_eq!(batch[0].code, rel::WHEEL);
        assert_eq!(batch[0].value, 1);
        assert_eq!(batch[1], InputEvent::SYN_REPORT);

        // Scrolling down gives a negative value in two's complement.
        c.on_wheel(MouseScrollDelta::LineDelta(0.0, -1.0));
        let batch = c.events().drain_batches().remove(0);
        assert_eq!(batch[0].value, (-1i32) as u32);

        // Sub-notch pixel deltas accumulate instead of vanishing.
        for _ in 0..3 {
            c.on_wheel(MouseScrollDelta::PixelDelta(PhysicalPosition::new(
                0.0, 10.0,
            )));
        }
        assert!(c.events().is_empty(), "30 px is under one notch");
        c.on_wheel(MouseScrollDelta::PixelDelta(PhysicalPosition::new(
            0.0, 10.0,
        )));
        let batch = c.events().drain_batches().remove(0);
        assert_eq!(batch[0].value, 1);

        // Horizontal scroll uses REL_HWHEEL.
        c.on_wheel(MouseScrollDelta::LineDelta(2.0, 0.0));
        let batch = c.events().drain_batches().remove(0);
        assert_eq!(batch[0].code, rel::HWHEEL);
        assert_eq!(batch[0].value, 2);
    }

    #[test]
    fn wheel_accumulator_ignores_bogus_deltas() {
        let mut acc = WheelAccumulator::default();
        assert_eq!(acc.add_lines(f64::NAN), 0);
        assert_eq!(acc.add_lines(f64::INFINITY), 0);
        assert_eq!(acc.add_lines(0.5), 0);
        assert_eq!(acc.add_lines(0.5), 1);
        assert_eq!(
            acc.add_pixels(WheelAccumulator::PIXELS_PER_NOTCH * -2.0),
            -2
        );
    }

    #[test]
    fn queue_drops_the_oldest_batch_when_the_guest_stalls() {
        let queue = InputQueue::new();
        let mut c = InputCapture::new(queue.clone(), ControlQueue::new());
        for _ in 0..(MAX_PENDING_BATCHES + 10) {
            press(&mut c, KeyCode::KeyA);
            release(&mut c, KeyCode::KeyA);
        }
        let stats = queue.stats();
        assert!(stats.dropped > 0);
        assert_eq!(queue.drain_batches().len(), MAX_PENDING_BATCHES);
        assert!(queue.is_empty());
        assert_eq!(stats.batches, 2 * (MAX_PENDING_BATCHES as u64 + 10));
    }

    #[test]
    fn drain_flattens_batches_in_order() {
        let mut c = capture();
        press(&mut c, KeyCode::KeyA);
        press(&mut c, KeyCode::KeyB);
        let flat = c.events().drain();
        assert_eq!(flat.len(), 4);
        assert_eq!(flat[0].code, 30);
        assert_eq!(flat[1], InputEvent::SYN_REPORT);
        assert_eq!(flat[2].code, 48);
        assert_eq!(flat[3], InputEvent::SYN_REPORT);
    }

    #[test]
    fn control_queue_is_bounded() {
        let q = ControlQueue::new();
        for _ in 0..(MAX_PENDING_CONTROL + 5) {
            q.push(ControlEvent::QuitRequested);
        }
        assert_eq!(q.drain().len(), MAX_PENDING_CONTROL);
    }
}
