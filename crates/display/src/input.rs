//! Host input capture: winit events → `virtio-input` event batches
//! (backlog MVP-902/904/905/906/907, host half of EPIC 9).
//!
//! The host never interprets keys, it only translates *positions*: winit
//! [`PhysicalKey::Code`] goes through [`crate::keymap`] to a Linux `KEY_*` code
//! and straight into the guest. Every group of events is terminated with
//! [`InputEvent::SYN_REPORT`] (MVP-905) and queued for the `virtio-input`
//! device, which drains it from its own thread.
//!
//! # The input grab (backlog MVP-907, WIN-1501/1502)
//!
//! Nothing is forwarded to the guest unless the *grab* is active: a VM window
//! that swallowed every keystroke as soon as it had focus would be unusable next
//! to other windows. The grab is engaged by clicking inside the guest image and
//! released by `Ctrl+Alt`, by `Ctrl+Alt+G`, or by losing focus. While it is
//! active the host cursor is hidden over the image ([`crate::ux::cursor_visible`])
//! because the guest draws its own.
//!
//! These shortcuts are consumed by the host and never reach the guest:
//!
//! | Shortcut | Effect |
//! |---|---|
//! | `Ctrl+Alt` (released with nothing pressed in between) | release the grab |
//! | `Ctrl+Alt+G` | toggle the grab ([`ControlEvent::GrabToggled`]) |
//! | `Ctrl+Alt+Q` | ask the VM to shut down ([`ControlEvent::QuitRequested`]) |
//! | `Ctrl+Alt+P` | freeze/continue the VM ([`ControlEvent::PauseToggleRequested`]) |
//! | `Ctrl+Alt+R` | reboot the VM in place ([`ControlEvent::ResetRequested`]) |
//! | `Ctrl+Alt+S` | suspend the VM to its snapshot file ([`ControlEvent::SaveRequested`]) |
//! | `F11` | toggle borderless fullscreen ([`WindowAction::ToggleFullscreen`]) |
//! | `Ctrl+Alt+O` | toggle 1:1 pixel mode ([`WindowAction::ToggleScaleMode`]) |
//!
//! Every *other* `Ctrl+Alt+<key>` combination is forwarded verbatim, so the
//! guest still gets `Ctrl+Alt+F2` (VT switch) and friends. The bare-modifier
//! release is only taken as "release the grab" when no other key or button was
//! pressed between the moment both modifiers went down and the moment the first
//! of them came up — which is what keeps the two rules from colliding.

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
    /// `Ctrl+Alt+P`: freeze the VM, or let a frozen one continue (ADR-0005).
    ///
    /// A *toggle* rather than a pair, because only the supervisor knows which
    /// state the VM is actually in — the window is one of several things that
    /// can pause it.
    PauseToggleRequested,
    /// `Ctrl+Alt+R`: reboot the VM in place (ADR-0005).
    ///
    /// The host-initiated peer of the guest pressing Restart: same machine
    /// reset, same process, same window — not a "reset button" that kills the
    /// VM and starts another one.
    ResetRequested,
    /// `Ctrl+Alt+S`: write the whole VM to its snapshot file and stop
    /// (ADR-0006).
    ///
    /// One-way, like closing a laptop lid: the VM freezes, its state goes to
    /// the file, and the process ends. `entangled resume <file>` is how it
    /// comes back — which is *another* process, with another window, so there
    /// is nothing for this one to toggle back to.
    SaveRequested,
}

/// A window-level effect the event loop has to apply (backlog EPIC 15).
///
/// Distinct from [`ControlEvent`]: these are instructions for the window itself,
/// produced synchronously by the event that caused them, whereas control events
/// are a queue the VM supervisor polls. Fullscreen and scaling never leave the
/// event loop at all.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum WindowAction {
    /// Apply this grab state: pointer confinement, cursor visibility, title.
    SetGrab(bool),
    /// `Ctrl+Alt+Q`: the user asked the VM to shut down.
    Quit,
    /// `Ctrl+Alt+P` / `Ctrl+Alt+R` / `Ctrl+Alt+S`: a lifecycle request the
    /// supervisor serves (ADR-0005, ADR-0006). The window itself does nothing
    /// but report it — pausing is not a window state, and neither is being
    /// written to a file.
    Lifecycle,
    /// `F11`: toggle borderless fullscreen (WIN-1504).
    ToggleFullscreen,
    /// `Ctrl+Alt+O`: switch between [`crate::ScaleMode`]s (WIN-1504).
    ToggleScaleMode,
}

/// What happened to one key press/release or button event.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum KeyOutcome {
    /// Forwarded to the guest as this Linux `KEY_*` code.
    Forwarded(u16),
    /// Swallowed by the host as a reserved shortcut; the event loop applies the
    /// [`WindowAction`].
    Reserved(WindowAction),
    /// Not forwarded because the input grab is inactive — the keystroke belongs
    /// to the host desktop, not to the guest (WIN-1502).
    Ungrabbed,
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

/// True for the two modifiers the grab shortcuts are built from. Both sides
/// count, and only these keys keep a pending `Ctrl+Alt` release alive.
fn is_grab_modifier(code: KeyCode) -> bool {
    matches!(
        code,
        KeyCode::ControlLeft | KeyCode::ControlRight | KeyCode::AltLeft | KeyCode::AltRight
    )
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

/// Translates winit input events into guest input batches and owns the input
/// grab state machine.
///
/// Free of window and GPU state, so the whole thing is unit-tested headlessly;
/// the event loop only feeds it and applies the [`WindowAction`]s it returns.
///
/// Two sets of held keys are tracked, and the difference matters:
///
/// - `held` — what the *user* is physically holding. Maintained whether or not
///   the grab is active, because that is what the reserved shortcuts have to be
///   recognised from.
/// - `guest_held` — what the *guest* was told is down, i.e. a subset of `held`
///   captured while grabbed. Only these ever get a key-up, which is what keeps
///   the guest free of stuck modifiers across grab and focus changes (MVP-906).
#[derive(Debug)]
pub struct InputCapture {
    events: InputQueue,
    control: ControlQueue,
    /// `EV_KEY` codes physically down — keyboard keys *and* mouse buttons, which
    /// share the `EV_KEY` code space in Linux.
    held: BTreeSet<u16>,
    /// `EV_KEY` codes the guest was told are down.
    guest_held: BTreeSet<u16>,
    grabbed: bool,
    focused: bool,
    /// True while `Ctrl+Alt` are both down and nothing else has been pressed
    /// since — the precondition for reading their release as "release the grab"
    /// (WIN-1502).
    modifiers_clean: bool,
    /// Last pointer position and the viewport it was mapped against, so a click
    /// knows whether it landed on the guest image and a fresh grab can re-sync
    /// the guest cursor without waiting for the next motion event.
    pointer: Option<(f64, f64)>,
    viewport: Option<Viewport>,
    /// Physical window size, for the edge margin of the cursor policy
    /// ([`crate::ux::near_window_edge`]); `None` before the window exists.
    window_size: Option<(u32, u32)>,
    /// Whether the pointer is inside the window at all (`CursorEntered`/`Left`).
    pointer_in_window: bool,
    vertical: WheelAccumulator,
    horizontal: WheelAccumulator,
    unmapped: u64,
}

impl InputCapture {
    /// Wires a capture to the queues the devices drain. The grab starts
    /// *inactive*: the user clicks into the image to hand input to the guest.
    pub fn new(events: InputQueue, control: ControlQueue) -> Self {
        Self {
            events,
            control,
            held: BTreeSet::new(),
            guest_held: BTreeSet::new(),
            grabbed: false,
            focused: false,
            modifiers_clean: false,
            pointer: None,
            viewport: None,
            window_size: None,
            pointer_in_window: false,
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

    /// Whether host input is currently routed to the guest.
    pub fn grabbed(&self) -> bool {
        self.grabbed
    }

    /// Whether the window currently has keyboard focus.
    pub fn focused(&self) -> bool {
        self.focused
    }

    /// Number of keys/buttons the guest currently believes are held.
    pub fn pressed_count(&self) -> usize {
        self.guest_held.len()
    }

    /// Number of keys/buttons the user is physically holding.
    pub fn held_count(&self) -> usize {
        self.held.len()
    }

    /// Keys seen that have no Linux equivalent (diagnostics).
    pub fn unmapped_keys(&self) -> u64 {
        self.unmapped
    }

    /// True when the last known pointer position lies on the guest image (not
    /// on a letterbox bar, and not within the window-edge margin) and the
    /// pointer is inside the window.
    ///
    /// The edge margin keeps the host cursor visible just before it crosses
    /// onto the window decorations, because after the crossing it can no
    /// longer be changed — see [`crate::ux::CURSOR_EDGE_MARGIN`].
    pub fn pointer_over_guest(&self) -> bool {
        if !self.pointer_in_window {
            return false;
        }
        let (Some((x, y)), Some(viewport)) = (self.pointer, self.viewport) else {
            return false;
        };
        if !viewport.contains(x, y) {
            return false;
        }
        match self.window_size {
            Some((w, h)) => !crate::ux::near_window_edge(x, y, w, h),
            None => true,
        }
    }

    /// Whether the *host* cursor should be visible right now (WIN-1501). The
    /// event loop mirrors this into `Window::set_cursor_visible`.
    pub fn cursor_visible(&self) -> bool {
        crate::ux::cursor_visible(self.grabbed, self.pointer_over_guest())
    }

    /// Keeps the viewport used for pointer mapping current across window resizes
    /// and scale-mode changes, without waiting for the next motion event.
    pub fn set_viewport(&mut self, viewport: Option<Viewport>) {
        self.viewport = viewport;
    }

    /// Keeps the physical window size used by the cursor policy's edge margin
    /// current; `None` while the window does not exist.
    pub fn set_window_size(&mut self, size: Option<(u32, u32)>) {
        self.window_size = size;
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
        match state {
            ElementState::Pressed => self.on_key_press(code),
            ElementState::Released => self.on_key_release(code),
        }
    }

    fn on_key_press(&mut self, code: KeyCode) -> KeyOutcome {
        // Reserved shortcuts are recognised *before* the key joins `held`, so
        // the modifier test only ever looks at the modifiers.
        if let Some(action) = self.reserved_shortcut(code) {
            // A reserved key is still "another key pressed in between", so the
            // modifier release that inevitably follows `Ctrl+Alt+O` (or F11 held
            // together with the modifiers) must not also release the grab.
            self.modifiers_clean = false;
            tracing::debug!(?action, ?code, "reserved shortcut consumed by the host");
            return KeyOutcome::Reserved(action);
        }
        let modifier = is_grab_modifier(code);
        let mapped = linux_keycode(code);
        if let Some(linux) = mapped {
            self.held.insert(linux);
        }
        // Any non-modifier press disqualifies the following modifier release
        // from being read as "release the grab" — that is what keeps
        // Ctrl+Alt+F2 working.
        self.modifiers_clean = if modifier {
            self.ctrl_down() && self.alt_down()
        } else {
            false
        };
        let Some(linux) = mapped else {
            self.unmapped += 1;
            tracing::trace!(?code, "no Linux keycode for this physical key");
            return KeyOutcome::Ignored;
        };
        if !self.grabbed {
            // Tracked, but the host desktop keeps the keystroke (WIN-1502).
            return KeyOutcome::Ungrabbed;
        }
        self.guest_held.insert(linux);
        self.emit_key(linux, KEY_DOWN);
        KeyOutcome::Forwarded(linux)
    }

    fn on_key_release(&mut self, code: KeyCode) -> KeyOutcome {
        let both_modifiers_were_down = self.ctrl_down() && self.alt_down();
        let modifier = is_grab_modifier(code);
        let mapped = linux_keycode(code);
        if let Some(linux) = mapped {
            self.held.remove(&linux);
        }
        if modifier {
            let releases_grab = self.grabbed && self.modifiers_clean && both_modifiers_were_down;
            self.modifiers_clean = false;
            if releases_grab {
                tracing::debug!("Ctrl+Alt released with nothing in between: releasing the grab");
                return KeyOutcome::Reserved(self.set_grab(false));
            }
        }
        let Some(linux) = mapped else {
            self.unmapped += 1;
            return KeyOutcome::Ignored;
        };
        if !self.guest_held.remove(&linux) {
            // Either the grab is inactive, or the guest already got this key-up
            // (focus was lost, or the grab was released, while it was held).
            return if self.grabbed {
                KeyOutcome::Ignored
            } else {
                KeyOutcome::Ungrabbed
            };
        }
        self.emit_key(linux, KEY_UP);
        KeyOutcome::Forwarded(linux)
    }

    /// Handles a pointer move. `viewport` is `None` while the window is
    /// minimized. The position is always remembered (the cursor-visibility and
    /// click-to-grab decisions need it); it is only *forwarded* while grabbed.
    pub fn on_pointer(&mut self, viewport: Option<Viewport>, x: f64, y: f64) -> bool {
        self.viewport = viewport;
        if !x.is_finite() || !y.is_finite() {
            return false;
        }
        self.pointer = Some((x, y));
        self.pointer_in_window = true;
        if viewport.is_none() || !self.grabbed {
            return false;
        }
        self.emit_pointer();
        true
    }

    /// The pointer entered or left the window (`CursorEntered`/`CursorLeft`).
    /// Only affects host cursor visibility; the guest is not told, because a
    /// pointer that left the window has no position to report.
    pub fn on_pointer_in_window(&mut self, inside: bool) {
        self.pointer_in_window = inside;
        if !inside {
            self.pointer = None;
        }
    }

    /// Handles a mouse button. Unknown buttons are dropped.
    ///
    /// While the grab is inactive, a click on the guest image engages it
    /// (WIN-1502) and is consumed — the guest must not see a click the user
    /// aimed at the window, and the position is re-synced so the guest cursor
    /// lands under the host one.
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
                self.held.insert(code);
                self.modifiers_clean = false;
                if !self.grabbed {
                    if !self.click_engages_grab() {
                        return KeyOutcome::Ungrabbed;
                    }
                    tracing::debug!("click on the guest image: grabbing input");
                    return KeyOutcome::Reserved(self.set_grab(true));
                }
                self.guest_held.insert(code);
                self.emit_key(code, KEY_DOWN);
            }
            ElementState::Released => {
                self.held.remove(&code);
                if !self.guest_held.remove(&code) {
                    return if self.grabbed {
                        KeyOutcome::Ignored
                    } else {
                        KeyOutcome::Ungrabbed
                    };
                }
                self.emit_key(code, KEY_UP);
            }
        }
        KeyOutcome::Forwarded(code)
    }

    /// Whether a click at the last known position should engage the grab: only
    /// over the guest image, never over the letterbox bars. A click with no known
    /// position (a compositor that sends the button before the first motion)
    /// counts as being over the image — the user did click into the window.
    fn click_engages_grab(&self) -> bool {
        match (self.pointer, self.viewport) {
            (Some((x, y)), Some(viewport)) => viewport.contains(x, y),
            _ => true,
        }
    }

    /// Handles a scroll event, emitting whole `REL_WHEEL`/`REL_HWHEEL` notches.
    /// Dropped entirely while the grab is inactive: scrolling belongs to whatever
    /// host window is under the pointer.
    pub fn on_wheel(&mut self, delta: MouseScrollDelta) {
        if !self.grabbed {
            return;
        }
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

    /// Handles focus changes. Losing focus drops the grab and releases every key
    /// and button the guest believes is held, so it cannot end up with a stuck
    /// modifier (MVP-906).
    pub fn on_focus(&mut self, focused: bool) {
        self.focused = focused;
        if focused {
            return;
        }
        let released = self.guest_held.len();
        let was_grabbed = self.grabbed;
        self.release_all();
        self.held.clear();
        self.grabbed = false;
        self.modifiers_clean = false;
        self.pointer_in_window = false;
        self.pointer = None;
        if released > 0 {
            tracing::debug!(released, "focus lost: released all held keys");
        }
        if was_grabbed {
            tracing::debug!("focus lost: grab released");
            self.control.push(ControlEvent::GrabToggled(false));
        }
    }

    /// Emits key-up for everything the guest believes is held, in one batch.
    pub fn release_all(&mut self) {
        self.vertical = WheelAccumulator::default();
        self.horizontal = WheelAccumulator::default();
        if self.guest_held.is_empty() {
            return;
        }
        let mut batch = Vec::with_capacity(self.guest_held.len() + 1);
        for code in std::mem::take(&mut self.guest_held) {
            batch.push(InputEvent {
                event_type: ev::KEY,
                code,
                value: KEY_UP,
            });
        }
        self.push(batch);
    }

    /// Applies a new grab state, queues the matching [`ControlEvent`] and returns
    /// the action for the event loop. Engaging the grab re-syncs the guest cursor
    /// to the host position; releasing it hands every held key back to the guest
    /// as a key-up, otherwise the guest keeps a stuck `Ctrl+Alt`.
    fn set_grab(&mut self, grabbed: bool) -> WindowAction {
        self.grabbed = grabbed;
        self.modifiers_clean = false;
        if grabbed {
            self.emit_pointer();
        } else {
            self.release_all();
        }
        self.control.push(ControlEvent::GrabToggled(grabbed));
        tracing::info!(grabbed, "input grab changed");
        WindowAction::SetGrab(grabbed)
    }

    /// Reserved shortcuts on key *press*, using our own held set as the modifier
    /// state — winit's `ModifiersChanged` can go stale across focus changes, our
    /// set cannot (focus loss clears it).
    fn reserved_shortcut(&mut self, code: KeyCode) -> Option<WindowAction> {
        // Fullscreen is reserved unconditionally: a bare F11 must not reach the
        // guest, or the user could lose the only way back out of fullscreen.
        if code == KeyCode::F11 {
            return Some(WindowAction::ToggleFullscreen);
        }
        if !self.ctrl_down() || !self.alt_down() {
            return None;
        }
        match code {
            KeyCode::KeyG => Some(self.set_grab(!self.grabbed)),
            KeyCode::KeyQ => {
                // The modifiers were forwarded while grabbed; hand them back so
                // a guest that survives the request has no stuck Ctrl+Alt.
                self.release_all();
                self.control.push(ControlEvent::QuitRequested);
                Some(WindowAction::Quit)
            }
            KeyCode::KeyO => Some(WindowAction::ToggleScaleMode),
            // Pause and reset (ADR-0005). Neither collides with anything the
            // guest is likely to want: `Ctrl+Alt+P` and `Ctrl+Alt+R` are not
            // VT switches (those are the function keys, still forwarded), and
            // both are consumed here so a paused guest cannot see half a
            // chord. As with `Ctrl+Alt+Q`, the modifiers are handed back first
            // so the guest is not left holding them across the freeze.
            KeyCode::KeyP => {
                self.release_all();
                self.control.push(ControlEvent::PauseToggleRequested);
                Some(WindowAction::Lifecycle)
            }
            KeyCode::KeyR => {
                self.release_all();
                self.control.push(ControlEvent::ResetRequested);
                Some(WindowAction::Lifecycle)
            }
            // Suspend (ADR-0006). Reserved on the same terms as its neighbours
            // and for a stronger reason: the guest is about to be frozen for
            // good, so it must not be left holding a modifier it can never see
            // released.
            KeyCode::KeyS => {
                self.release_all();
                self.control.push(ControlEvent::SaveRequested);
                Some(WindowAction::Lifecycle)
            }
            _ => None,
        }
    }

    fn ctrl_down(&self) -> bool {
        self.held.contains(&KEY_LEFTCTRL) || self.held.contains(&KEY_RIGHTCTRL)
    }

    fn alt_down(&self) -> bool {
        self.held.contains(&KEY_LEFTALT) || self.held.contains(&KEY_RIGHTALT)
    }

    /// Sends the last known pointer position to the guest, so its cursor sits
    /// under the host cursor the moment the grab starts.
    fn emit_pointer(&mut self) {
        let (Some((x, y)), Some(viewport)) = (self.pointer, self.viewport) else {
            return;
        };
        let [abs_x, abs_y] = pointer_abs_events(&viewport, x, y);
        self.push(vec![abs_x, abs_y]);
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

    /// A 1920x1080 image with 100 px pillarbox bars on both sides.
    const VIEW: Viewport = Viewport {
        x: 100,
        y: 0,
        width: 1920,
        height: 1080,
    };

    fn capture() -> InputCapture {
        InputCapture::new(InputQueue::new(), ControlQueue::new())
    }

    /// A capture with the grab engaged the way a user does it: move the pointer
    /// onto the image, click. Queues are drained, so a test sees only its own
    /// events.
    fn grabbed_capture() -> InputCapture {
        let mut c = capture();
        c.on_focus(true);
        c.on_pointer(Some(VIEW), 1060.0, 540.0);
        assert_eq!(
            c.on_button(MouseButton::Left, ElementState::Pressed),
            KeyOutcome::Reserved(WindowAction::SetGrab(true))
        );
        assert_eq!(
            c.on_button(MouseButton::Left, ElementState::Released),
            KeyOutcome::Ignored,
            "the release of the grabbing click is not forwarded either"
        );
        assert!(c.grabbed());
        let _ = c.events().drain();
        let _ = c.control().drain();
        c
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

    fn hold_ctrl_alt(c: &mut InputCapture) {
        press(c, KeyCode::ControlLeft);
        press(c, KeyCode::AltLeft);
    }

    #[test]
    fn key_press_and_release_are_syn_terminated() {
        let mut c = grabbed_capture();
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
        assert_eq!(c.held_count(), 0);
    }

    #[test]
    fn repeats_and_unmapped_keys_are_dropped() {
        let mut c = grabbed_capture();
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
        let mut c = grabbed_capture();
        assert_eq!(release(&mut c, KeyCode::KeyA), KeyOutcome::Ignored);
        assert!(c.events().is_empty());
    }

    #[test]
    fn focus_loss_releases_every_held_key() {
        let mut c = grabbed_capture();
        press(&mut c, KeyCode::ControlLeft);
        press(&mut c, KeyCode::ShiftLeft);
        press(&mut c, KeyCode::KeyA);
        c.on_button(MouseButton::Left, ElementState::Pressed);
        assert_eq!(c.pressed_count(), 4);
        let _ = c.events().drain();

        c.on_focus(false);
        assert_eq!(c.pressed_count(), 0);
        assert_eq!(c.held_count(), 0);
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
        assert_eq!(release(&mut c, KeyCode::KeyA), KeyOutcome::Ungrabbed);
        assert!(c.events().is_empty());
    }

    #[test]
    fn focus_loss_drops_the_grab() {
        let mut c = grabbed_capture();
        assert!(c.grabbed());
        c.on_focus(false);
        assert!(!c.grabbed());
        assert!(!c.focused());
        assert_eq!(c.control().drain(), vec![ControlEvent::GrabToggled(false)]);
        // Focus loss without a grab does not announce anything.
        c.on_focus(true);
        c.on_focus(false);
        assert!(c.control().drain().is_empty());
    }

    // ---- WIN-1502: what reaches the guest, and what does not ----------------

    #[test]
    fn keyboard_is_not_forwarded_while_ungrabbed() {
        let mut c = capture();
        c.on_focus(true);
        assert_eq!(press(&mut c, KeyCode::KeyA), KeyOutcome::Ungrabbed);
        assert_eq!(release(&mut c, KeyCode::KeyA), KeyOutcome::Ungrabbed);
        assert!(c.events().is_empty(), "the guest sees nothing");
        // The physical state is still tracked, which is what shortcuts need.
        press(&mut c, KeyCode::ControlLeft);
        assert_eq!(c.held_count(), 1);
        assert_eq!(c.pressed_count(), 0);
    }

    #[test]
    fn pointer_and_wheel_are_not_forwarded_while_ungrabbed() {
        let mut c = capture();
        assert!(!c.on_pointer(Some(VIEW), 500.0, 500.0));
        c.on_wheel(MouseScrollDelta::LineDelta(0.0, 3.0));
        assert!(c.events().is_empty());
        // ... but the position is remembered for the cursor policy.
        assert!(c.pointer_over_guest());
    }

    #[test]
    fn a_click_on_the_guest_image_grabs_and_syncs_the_pointer() {
        let mut c = capture();
        c.on_pointer(Some(VIEW), 1060.0, 540.0);
        assert!(c.events().is_empty(), "nothing forwarded before the grab");
        assert_eq!(
            c.on_button(MouseButton::Left, ElementState::Pressed),
            KeyOutcome::Reserved(WindowAction::SetGrab(true))
        );
        assert!(c.grabbed());
        assert_eq!(c.control().drain(), vec![ControlEvent::GrabToggled(true)]);
        // The guest is told where the pointer is, and never sees the click.
        let batch = c.events().drain_batches().remove(0);
        assert_eq!(batch[0].code, abs::X);
        assert_eq!(batch[1].code, abs::Y);
        assert_eq!(batch[2], InputEvent::SYN_REPORT);
        let half = virtio_input::ABS_AXIS_MAX / 2;
        assert!(batch[0].value.abs_diff(half) <= 1);
        assert!(batch[1].value.abs_diff(half) <= 1);
        assert!(c.events().is_empty());
        assert_eq!(c.pressed_count(), 0, "BTN_LEFT was not forwarded");
        assert_eq!(c.held_count(), 1, "but it is physically down");
    }

    #[test]
    fn a_click_on_the_letterbox_bars_does_not_grab() {
        let mut c = capture();
        c.on_pointer(Some(VIEW), 40.0, 540.0);
        assert!(!c.pointer_over_guest());
        assert_eq!(
            c.on_button(MouseButton::Left, ElementState::Pressed),
            KeyOutcome::Ungrabbed
        );
        assert!(!c.grabbed());
        assert_eq!(
            c.on_button(MouseButton::Left, ElementState::Released),
            KeyOutcome::Ungrabbed
        );
        assert!(c.events().is_empty());
        assert!(c.control().drain().is_empty());
    }

    #[test]
    fn a_click_with_no_known_position_still_grabs() {
        let mut c = capture();
        assert_eq!(
            c.on_button(MouseButton::Left, ElementState::Pressed),
            KeyOutcome::Reserved(WindowAction::SetGrab(true))
        );
        assert!(c.grabbed());
        assert!(c.events().is_empty(), "no position to sync");
    }

    #[test]
    fn ctrl_alt_releases_the_grab_in_either_order() {
        for second in [KeyCode::AltLeft, KeyCode::ControlLeft] {
            let mut c = grabbed_capture();
            press(&mut c, KeyCode::ControlLeft);
            press(&mut c, KeyCode::AltLeft);
            let _ = c.events().drain();
            assert_eq!(
                release(&mut c, second),
                KeyOutcome::Reserved(WindowAction::SetGrab(false))
            );
            assert!(!c.grabbed());
            assert_eq!(c.control().drain(), vec![ControlEvent::GrabToggled(false)]);
            // Both modifiers were handed back to the guest as key-ups.
            let released: Vec<(u16, u32)> = c
                .events()
                .drain()
                .iter()
                .filter(|e| e.event_type == ev::KEY)
                .map(|e| (e.code, e.value))
                .collect();
            assert_eq!(released, vec![(KEY_LEFTCTRL, 0), (KEY_LEFTALT, 0)]);
            // Releasing the other modifier afterwards is a no-op.
            let other = if second == KeyCode::AltLeft {
                KeyCode::ControlLeft
            } else {
                KeyCode::AltLeft
            };
            assert_eq!(release(&mut c, other), KeyOutcome::Ungrabbed);
            assert!(c.events().is_empty());
            assert_eq!(c.held_count(), 0);
        }
    }

    #[test]
    fn right_hand_modifiers_release_the_grab_too() {
        let mut c = grabbed_capture();
        press(&mut c, KeyCode::ControlRight);
        press(&mut c, KeyCode::AltRight);
        assert_eq!(
            release(&mut c, KeyCode::AltRight),
            KeyOutcome::Reserved(WindowAction::SetGrab(false))
        );
        assert!(!c.grabbed());
        // Mixed sides work as well.
        let mut c = grabbed_capture();
        press(&mut c, KeyCode::ControlLeft);
        press(&mut c, KeyCode::AltRight);
        assert_eq!(
            release(&mut c, KeyCode::ControlLeft),
            KeyOutcome::Reserved(WindowAction::SetGrab(false))
        );
    }

    #[test]
    fn ctrl_alt_plus_a_third_key_reaches_the_guest_and_keeps_the_grab() {
        let mut c = grabbed_capture();
        hold_ctrl_alt(&mut c);
        // Ctrl+Alt+F2 is the guest's VT switch, not ours.
        assert_eq!(press(&mut c, KeyCode::F2), KeyOutcome::Forwarded(60));
        assert_eq!(release(&mut c, KeyCode::F2), KeyOutcome::Forwarded(60));
        assert_eq!(release(&mut c, KeyCode::AltLeft), KeyOutcome::Forwarded(56));
        assert!(c.grabbed(), "the grab survives Ctrl+Alt+F2");
        assert_eq!(
            release(&mut c, KeyCode::ControlLeft),
            KeyOutcome::Forwarded(29)
        );
        assert!(c.grabbed());
        assert!(c.control().drain().is_empty());
    }

    #[test]
    fn a_reserved_third_key_also_keeps_the_grab() {
        // Regression (found under WSLg): `Ctrl+Alt+O` toggled the scale mode and
        // then the modifier release dropped the grab as a side effect.
        for third in [KeyCode::KeyO, KeyCode::F11] {
            let mut c = grabbed_capture();
            hold_ctrl_alt(&mut c);
            assert!(matches!(press(&mut c, third), KeyOutcome::Reserved(_)));
            assert_eq!(release(&mut c, KeyCode::AltLeft), KeyOutcome::Forwarded(56));
            assert!(c.grabbed(), "{third:?} must not release the grab");
            assert_eq!(
                release(&mut c, KeyCode::ControlLeft),
                KeyOutcome::Forwarded(29)
            );
            assert!(c.grabbed());
        }
    }

    #[test]
    fn ctrl_alt_g_engaging_the_grab_survives_the_modifier_release() {
        let mut c = capture();
        hold_ctrl_alt(&mut c);
        assert_eq!(
            press(&mut c, KeyCode::KeyG),
            KeyOutcome::Reserved(WindowAction::SetGrab(true))
        );
        // The user now lets go of Ctrl+Alt: the grab they just asked for stays.
        assert_eq!(release(&mut c, KeyCode::AltLeft), KeyOutcome::Ignored);
        assert_eq!(release(&mut c, KeyCode::ControlLeft), KeyOutcome::Ignored);
        assert!(c.grabbed());
    }

    #[test]
    fn a_click_between_the_modifiers_also_keeps_the_grab() {
        let mut c = grabbed_capture();
        hold_ctrl_alt(&mut c);
        c.on_button(MouseButton::Left, ElementState::Pressed);
        assert_eq!(release(&mut c, KeyCode::AltLeft), KeyOutcome::Forwarded(56));
        assert!(c.grabbed());
    }

    #[test]
    fn one_modifier_alone_never_releases_the_grab() {
        let mut c = grabbed_capture();
        press(&mut c, KeyCode::ControlLeft);
        assert_eq!(
            release(&mut c, KeyCode::ControlLeft),
            KeyOutcome::Forwarded(29)
        );
        assert!(c.grabbed());
        press(&mut c, KeyCode::AltLeft);
        assert_eq!(release(&mut c, KeyCode::AltLeft), KeyOutcome::Forwarded(56));
        assert!(c.grabbed());
    }

    #[test]
    fn ctrl_alt_release_is_inert_while_ungrabbed() {
        let mut c = capture();
        hold_ctrl_alt(&mut c);
        assert_eq!(release(&mut c, KeyCode::AltLeft), KeyOutcome::Ungrabbed);
        assert!(!c.grabbed());
        assert!(c.control().drain().is_empty());
        assert!(c.events().is_empty());
    }

    #[test]
    fn a_second_ctrl_alt_release_needs_the_modifiers_pressed_again() {
        let mut c = grabbed_capture();
        hold_ctrl_alt(&mut c);
        assert!(matches!(
            release(&mut c, KeyCode::AltLeft),
            KeyOutcome::Reserved(WindowAction::SetGrab(false))
        ));
        // Re-grab, then release the still-held Ctrl: nothing to trigger on.
        c.on_pointer(Some(VIEW), 1060.0, 540.0);
        c.on_button(MouseButton::Left, ElementState::Pressed);
        assert!(c.grabbed());
        assert_eq!(release(&mut c, KeyCode::ControlLeft), KeyOutcome::Ignored);
        assert!(c.grabbed());
    }

    // ---- Reserved shortcut routing ----------------------------------------

    #[test]
    fn reserved_shortcuts_never_reach_the_guest() {
        let mut c = grabbed_capture();
        hold_ctrl_alt(&mut c);
        let _ = c.events().drain();

        assert_eq!(
            press(&mut c, KeyCode::KeyG),
            KeyOutcome::Reserved(WindowAction::SetGrab(false))
        );
        assert!(!c.grabbed());
        // Ctrl and Alt were released towards the guest, KEY_G (34) never sent.
        let sent: Vec<u16> = c
            .events()
            .drain()
            .iter()
            .filter(|e| e.event_type == ev::KEY)
            .map(|e| e.code)
            .collect();
        assert!(!sent.contains(&34));
        assert_eq!(sent, vec![KEY_LEFTCTRL, KEY_LEFTALT]);
        assert_eq!(c.pressed_count(), 0);

        // The modifiers are still physically down, so G toggles straight back.
        assert_eq!(
            press(&mut c, KeyCode::KeyG),
            KeyOutcome::Reserved(WindowAction::SetGrab(true))
        );
        assert!(c.grabbed());

        assert_eq!(
            press(&mut c, KeyCode::KeyQ),
            KeyOutcome::Reserved(WindowAction::Quit)
        );
        assert_eq!(
            press(&mut c, KeyCode::KeyO),
            KeyOutcome::Reserved(WindowAction::ToggleScaleMode)
        );
        assert_eq!(
            c.control().drain(),
            vec![
                ControlEvent::GrabToggled(false),
                ControlEvent::GrabToggled(true),
                ControlEvent::QuitRequested
            ],
            "fullscreen and scaling stay inside the event loop"
        );
    }

    #[test]
    fn reserved_shortcuts_work_without_a_grab_too() {
        let mut c = capture();
        hold_ctrl_alt(&mut c);
        assert_eq!(
            press(&mut c, KeyCode::KeyQ),
            KeyOutcome::Reserved(WindowAction::Quit)
        );
        assert_eq!(
            press(&mut c, KeyCode::KeyG),
            KeyOutcome::Reserved(WindowAction::SetGrab(true))
        );
        assert!(c.grabbed());
        assert!(c.events().is_empty(), "no pointer position to sync");
    }

    #[test]
    fn f11_is_reserved_with_or_without_modifiers() {
        let mut c = grabbed_capture();
        assert_eq!(
            press(&mut c, KeyCode::F11),
            KeyOutcome::Reserved(WindowAction::ToggleFullscreen)
        );
        assert_eq!(release(&mut c, KeyCode::F11), KeyOutcome::Ignored);
        hold_ctrl_alt(&mut c);
        assert_eq!(
            press(&mut c, KeyCode::F11),
            KeyOutcome::Reserved(WindowAction::ToggleFullscreen)
        );
        // F11 never appears in the guest stream (KEY_F11 == 87).
        let sent: Vec<u16> = c
            .events()
            .drain()
            .iter()
            .filter(|e| e.event_type == ev::KEY)
            .map(|e| e.code)
            .collect();
        assert!(!sent.contains(&87), "{sent:?}");
        assert!(c.grabbed(), "fullscreen does not touch the grab");
    }

    #[test]
    fn g_q_and_o_reach_the_guest_without_both_modifiers() {
        let mut c = grabbed_capture();
        assert_eq!(press(&mut c, KeyCode::KeyG), KeyOutcome::Forwarded(34));
        press(&mut c, KeyCode::ControlLeft);
        // Ctrl alone is not the shortcut.
        assert_eq!(press(&mut c, KeyCode::KeyQ), KeyOutcome::Forwarded(16));
        assert_eq!(press(&mut c, KeyCode::KeyO), KeyOutcome::Forwarded(24));
        assert!(c.grabbed());
        assert!(c.control().drain().is_empty());
    }

    // ---- WIN-1501: host cursor visibility ---------------------------------

    #[test]
    fn the_cursor_hides_only_over_a_grabbed_guest_image() {
        let mut c = capture();
        assert!(c.cursor_visible(), "no grab, no pointer: visible");
        c.on_pointer(Some(VIEW), 1060.0, 540.0);
        assert!(
            c.cursor_visible(),
            "ungrabbed over the image: still visible"
        );

        c.on_button(MouseButton::Left, ElementState::Pressed);
        assert!(c.grabbed());
        assert!(!c.cursor_visible(), "grabbed over the image: hidden");

        // Onto the letterbox bar: the user needs the cursor back.
        c.on_pointer(Some(VIEW), 20.0, 540.0);
        assert!(c.cursor_visible());
        c.on_pointer(Some(VIEW), 1060.0, 540.0);
        assert!(!c.cursor_visible());

        // Leaving the window shows it again, and entering does not hide it until
        // the position is known again.
        c.on_pointer_in_window(false);
        assert!(c.cursor_visible());
        c.on_pointer_in_window(true);
        assert!(c.cursor_visible());
        c.on_pointer(Some(VIEW), 1060.0, 540.0);
        assert!(!c.cursor_visible());

        // Releasing the grab restores it wherever the pointer is.
        hold_ctrl_alt(&mut c);
        release(&mut c, KeyCode::AltLeft);
        assert!(c.cursor_visible());
    }

    #[test]
    fn the_cursor_stays_visible_within_the_window_edge_margin() {
        let mut c = grabbed_capture();
        // A viewport that fills the whole window: the guest image touches every
        // window edge, as a maximized fit-mode window does.
        let viewport = Viewport {
            x: 0,
            y: 0,
            width: 2120,
            height: 1080,
        };
        c.set_window_size(Some((2120, 1080)));
        c.on_pointer(Some(viewport), 1060.0, 540.0);
        assert!(!c.cursor_visible(), "window center: hidden as before");
        // Approaching the top edge (towards the CSD titlebar): the cursor must
        // come back *before* it crosses onto the frame, where it can no longer
        // be changed (WSLg).
        c.on_pointer(Some(viewport), 1060.0, 4.0);
        assert!(c.cursor_visible(), "top margin");
        c.on_pointer(Some(viewport), 4.0, 540.0);
        assert!(c.cursor_visible(), "left margin");
        c.on_pointer(Some(viewport), 2116.0, 540.0);
        assert!(c.cursor_visible(), "right margin");
        c.on_pointer(Some(viewport), 1060.0, 1076.0);
        assert!(c.cursor_visible(), "bottom margin");
        // Without a known window size the policy falls back to the viewport
        // alone (headless tests, no window yet).
        c.set_window_size(None);
        c.on_pointer(Some(viewport), 1060.0, 4.0);
        assert!(!c.cursor_visible());
    }

    #[test]
    fn a_resize_updates_the_viewport_used_by_the_cursor_policy() {
        let mut c = grabbed_capture();
        assert!(!c.cursor_visible());
        // The window shrank: the same position now sits outside the image.
        c.set_viewport(Some(Viewport {
            x: 0,
            y: 0,
            width: 320,
            height: 180,
        }));
        assert!(c.cursor_visible());
        c.set_viewport(None);
        assert!(c.cursor_visible(), "minimized: nothing to hide over");
    }

    // ---- Pointer mapping (unchanged geometry, now gated by the grab) -------

    #[test]
    fn pointer_maps_into_the_viewport_and_clamps() {
        let mut c = grabbed_capture();
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
        let mut c = grabbed_capture();
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
        let mut c = grabbed_capture();
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
        let mut c = grabbed_capture();
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
        let mut c = grabbed_capture();
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
    fn releasing_the_grab_forgets_the_accumulated_scroll() {
        let mut c = grabbed_capture();
        c.on_wheel(MouseScrollDelta::PixelDelta(PhysicalPosition::new(
            0.0, 20.0,
        )));
        assert!(c.events().is_empty(), "half a notch is pending");
        hold_ctrl_alt(&mut c);
        release(&mut c, KeyCode::AltLeft);
        let _ = c.events().drain();
        c.on_pointer(Some(VIEW), 1060.0, 540.0);
        c.on_button(MouseButton::Left, ElementState::Pressed);
        let _ = c.events().drain();
        c.on_wheel(MouseScrollDelta::PixelDelta(PhysicalPosition::new(
            0.0, 20.0,
        )));
        assert!(
            c.events().is_empty(),
            "the stale half notch did not carry over"
        );
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
        c.on_button(MouseButton::Left, ElementState::Pressed);
        c.on_button(MouseButton::Left, ElementState::Released);
        assert!(c.grabbed());
        let _ = queue.drain_batches();
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
        let mut c = grabbed_capture();
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
