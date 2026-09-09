//! The Linux half of host gamepad capture: `/dev/input/event*`, read directly.
//!
//! # Why hand-rolled
//!
//! The obvious crate for this is `gilrs`, and its own licence (Apache-2.0) is
//! fine — but it pulls in a stack (`nix`, `libudev`-style discovery, a
//! platform abstraction for four operating systems) that `cargo deny` would
//! have to be re-audited against on every bump, to do something this crate
//! already does everywhere else: read a `#[repr(C)]` struct out of a file
//! descriptor. virtio-input already parses `struct virtio_input_event`; this
//! is `struct input_event`, which is the same eight fields with a timestamp in
//! front. So: `libc`, four `ioctl`s and a `poll`.
//!
//! # What it does
//!
//! One controller at a time — player one. Every [`RESCAN_INTERVAL`] the source
//! walks `/dev/input`, opens anything it can that looks like a gamepad, and
//! adopts the lowest-numbered one. While a pad is adopted the source sits in
//! `poll(2)`, folding `EV_KEY`/`EV_ABS` reports into a [`PadState`]. A pad that
//! vanishes (`ENODEV`, or a read of 0 bytes) is dropped, and the pump turns
//! that into a full release for the guest.
//!
//! Axis values are rescaled from the host device's own `EVIOCGABS` range onto
//! the range the guest device advertises — the one thing that has to happen
//! here, because an Xbox pad's triggers run `0..255` and an Xbox One pad's run
//! `0..1023`, and a guest told the range is `0..255` would see a quarter-pull
//! as a full one.

use std::collections::BTreeSet;
use std::ffi::CString;
use std::os::fd::{AsRawFd, FromRawFd, OwnedFd, RawFd};
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

use super::{to_hat, to_stick, to_trigger, GamepadSource, PadId, PadState, Poll, RESCAN_INTERVAL};
use crate::{abs, btn, ev};

/// Where the kernel puts evdev nodes.
const INPUT_DIR: &str = "/dev/input";

/// Cap on the device nodes one scan will look at. `/dev/input` is host-owned,
/// not guest-owned, so this is a sanity bound rather than a security one — but
/// a scan that runs every second must not become unbounded because something
/// created ten thousand nodes.
pub const MAX_SCANNED_DEVICES: usize = 64;

/// Cap on the `input_event` records folded in per poll. A pad reporting at
/// 1 kHz produces eight per tick; anything past this is a device gone mad, and
/// the leftovers are read on the next tick anyway.
pub const MAX_EVENTS_PER_POLL: usize = 256;

/// Bytes of `EV_KEY` bitmap fetched. Covers codes 0..=767, which reaches past
/// `BTN_DPAD_RIGHT` (0x223 = 547).
const KEY_BITMAP_BYTES: usize = 96;
/// Bytes of `EV_ABS` bitmap fetched: codes 0..=63, i.e. all of `ABS_MAX`.
const ABS_BITMAP_BYTES: usize = 8;
/// Bytes of device name fetched.
const NAME_BYTES: usize = 128;

/// The name this VMM gives its *guest* pad. Skipped during the scan so that
/// running Entangled inside an Entangled guest does not feed a pad back into
/// itself.
const OWN_DEVICE_NAME: &str = "Entangled Gamepad";

// ------------------------------------------------------------------ ioctls

/// `_IOC(_IOC_READ, 'E', nr, size)` — every ioctl here is a read.
///
/// `libc::Ioctl` is `i32` on musl and `u64` on glibc, and the value has bit 31
/// set, so the cast is spelled out once here rather than at four call sites.
const fn evioc_read(nr: u32, size: u32) -> libc::Ioctl {
    #[allow(clippy::unnecessary_cast)]
    (((2u32 << 30) | (size << 16) | ((b'E' as u32) << 8) | nr) as libc::Ioctl)
}

/// `EVIOCGID`: `struct input_id`.
fn eviocgid() -> libc::Ioctl {
    evioc_read(0x02, std::mem::size_of::<InputId>() as u32)
}
/// `EVIOCGNAME(len)`.
fn eviocgname(len: usize) -> libc::Ioctl {
    evioc_read(0x06, len as u32)
}
/// `EVIOCGKEY(len)`: the *current* state of every key, so a pad adopted with a
/// button already held does not look released until it is let go.
fn eviocgkey(len: usize) -> libc::Ioctl {
    evioc_read(0x18, len as u32)
}
/// `EVIOCGBIT(ev, len)`: which codes of event type `ev` the device can emit.
fn eviocgbit(event_type: u16, len: usize) -> libc::Ioctl {
    evioc_read(0x20 + u32::from(event_type), len as u32)
}
/// `EVIOCGABS(axis)`: `struct input_absinfo`, including the axis's *current*
/// value, which is how an adopted pad's sticks start out in the right place.
fn eviocgabs(axis: u16) -> libc::Ioctl {
    evioc_read(
        0x40 + u32::from(axis),
        std::mem::size_of::<AbsInfo>() as u32,
    )
}

/// `struct input_id` from `linux/input.h`.
#[repr(C)]
#[derive(Debug, Default, Clone, Copy)]
struct InputId {
    bustype: u16,
    vendor: u16,
    product: u16,
    version: u16,
}

/// `struct input_absinfo` from `linux/input.h`.
#[repr(C)]
#[derive(Debug, Default, Clone, Copy)]
struct AbsInfo {
    value: i32,
    minimum: i32,
    maximum: i32,
    fuzz: i32,
    flat: i32,
    resolution: i32,
}

/// `struct input_event` from `linux/input.h`: a timestamp in front of exactly
/// the three fields `virtio_input_event` carries.
#[repr(C)]
#[derive(Debug, Default, Clone, Copy)]
struct RawEvent {
    time: libc::timeval,
    event_type: u16,
    code: u16,
    value: i32,
}

/// The `event*` nodes in `dir` a scan will look at: sorted, then capped at
/// [`MAX_SCANNED_DEVICES`].
///
/// Sorted **before** the cap, not after. Both orders keep the bound, but only
/// this one keeps the promise that goes with it: "player one" is the
/// lowest-numbered pad, and a `readdir` that happened to hand back
/// `event90..event160` first would otherwise silently pick a different
/// controller on a machine with a lot of input devices.
///
/// Takes the directory as an argument so the bound is testable without a
/// `/dev/input` full of fakes.
fn candidate_nodes(dir: &Path) -> Vec<PathBuf> {
    let Ok(entries) = std::fs::read_dir(dir) else {
        return Vec::new();
    };
    let mut nodes: Vec<PathBuf> = entries
        .flatten()
        .map(|entry| entry.path())
        .filter(|path| {
            path.file_name()
                .and_then(|name| name.to_str())
                .is_some_and(|name| name.starts_with("event"))
        })
        .collect();
    // Numeric, not lexical: `event2` comes before `event10`, which is what a
    // human means by "the first pad" and what `readdir` order does not give.
    nodes.sort_by_key(|path| {
        let number = path
            .file_name()
            .and_then(|name| name.to_str())
            .and_then(|name| name.strip_prefix("event"))
            .and_then(|digits| digits.parse::<u64>().ok())
            .unwrap_or(u64::MAX);
        (number, path.clone())
    });
    nodes.truncate(MAX_SCANNED_DEVICES);
    nodes
}

/// Reads `code`'s bit out of an evdev capability bitmap.
fn has_bit(bitmap: &[u8], code: u16) -> bool {
    let index = usize::from(code / 8);
    bitmap
        .get(index)
        .is_some_and(|byte| byte & (1u8 << (code % 8)) != 0)
}

// ------------------------------------------------------------------ source

/// The six analogue axes, in the order [`AdoptedPad::axis_range`] indexes them.
const ANALOGUE_AXES: [u16; 6] = [abs::X, abs::Y, abs::RX, abs::RY, abs::Z, abs::RZ];

/// One host controller the source has adopted.
struct AdoptedPad {
    fd: OwnedFd,
    path: PathBuf,
    label: String,
    /// The evdev node number, so two identically named pads are still distinct.
    slot: u64,
    /// `EVIOCGID`. Logged and nothing else — but "which controller did it
    /// adopt" is the first question of every bug report about a pad, and a
    /// bus/vendor/product triple answers it where a name like "Controller"
    /// does not.
    ids: InputId,
    /// `EVIOCGABS` min/max per entry of [`ANALOGUE_AXES`]; `None` for an axis
    /// the pad does not have.
    axis_range: [Option<(i32, i32)>; 6],
    /// True when the pad reports its triggers as `BTN_TL2`/`BTN_TR2` rather
    /// than `ABS_Z`/`ABS_RZ` — some third-party pads and every "digital
    /// trigger" gamepad do.
    digital_triggers: bool,
    /// True when the pad reports its D-pad as `BTN_DPAD_*` rather than a hat.
    digital_dpad: bool,
}

/// Host gamepad capture through evdev.
pub struct EvdevSource {
    pad: Option<AdoptedPad>,
    state: PadState,
    next_scan: Instant,
    /// Nodes whose `open` failed, so a permission problem is logged once
    /// instead of once a second for the life of the VM.
    complained: BTreeSet<PathBuf>,
}

impl Default for EvdevSource {
    fn default() -> Self {
        Self::new()
    }
}

impl EvdevSource {
    pub fn new() -> Self {
        Self {
            pad: None,
            state: PadState::NEUTRAL,
            // Scan on the very first poll rather than a second into the run.
            next_scan: Instant::now(),
            complained: BTreeSet::new(),
        }
    }

    /// Drops the adopted pad and re-centres the cached state.
    fn release(&mut self, reason: &str) {
        if let Some(pad) = self.pad.take() {
            tracing::debug!(path = %pad.path.display(), reason, "releasing host gamepad");
        }
        self.state = PadState::NEUTRAL;
    }

    /// Looks for a controller to adopt, at most once per [`RESCAN_INTERVAL`].
    fn scan(&mut self) {
        if self.pad.is_some() || Instant::now() < self.next_scan {
            return;
        }
        self.next_scan = Instant::now() + RESCAN_INTERVAL;

        for path in candidate_nodes(Path::new(INPUT_DIR)) {
            match self.try_adopt(&path) {
                Ok(Some(pad)) => {
                    tracing::info!(
                        path = %pad.path.display(),
                        controller = %pad.label,
                        bus = format_args!("{:04x}", pad.ids.bustype),
                        vendor = format_args!("{:04x}", pad.ids.vendor),
                        product = format_args!("{:04x}", pad.ids.product),
                        version = format_args!("{:04x}", pad.ids.version),
                        digital_triggers = pad.digital_triggers,
                        digital_dpad = pad.digital_dpad,
                        "adopted host gamepad"
                    );
                    self.state = seed_state(&pad);
                    self.pad = Some(pad);
                    return;
                }
                Ok(None) => {}
                Err(error) => {
                    if self.complained.insert(path.clone()) {
                        tracing::debug!(
                            path = %path.display(),
                            %error,
                            "cannot inspect input device (a gamepad here would not be usable)"
                        );
                    }
                }
            }
        }
    }

    /// Opens one node and decides whether it is a gamepad we can drive.
    fn try_adopt(&self, path: &Path) -> std::io::Result<Option<AdoptedPad>> {
        let fd = open_read_nonblock(path)?;
        let raw = fd.as_raw_fd();

        let name = ioctl_bytes(raw, eviocgname(NAME_BYTES), NAME_BYTES)
            .map(|bytes| {
                String::from_utf8_lossy(bytes.split(|&b| b == 0).next().unwrap_or(&[])).into_owned()
            })
            .unwrap_or_default();
        if name == OWN_DEVICE_NAME {
            return Ok(None);
        }

        let keys = ioctl_bytes(raw, eviocgbit(ev::KEY, KEY_BITMAP_BYTES), KEY_BITMAP_BYTES)
            .unwrap_or_default();
        let axes = ioctl_bytes(raw, eviocgbit(ev::ABS, ABS_BITMAP_BYTES), ABS_BITMAP_BYTES)
            .unwrap_or_default();

        // The same classification `joydev_match()` makes, for the same reason:
        // a touchscreen, a digitiser and a mouse all have absolute axes.
        let looks_like_a_pad = has_bit(&keys, btn::SOUTH) && has_bit(&axes, abs::X);
        let is_something_else = has_bit(&keys, btn::LEFT)      // BTN_MOUSE
            || has_bit(&keys, 0x14a)                            // BTN_TOUCH
            || has_bit(&keys, 0x140); // BTN_DIGI
        if !looks_like_a_pad || is_something_else {
            return Ok(None);
        }

        let mut axis_range = [None; 6];
        for (slot, &axis) in ANALOGUE_AXES.iter().enumerate() {
            if !has_bit(&axes, axis) {
                continue;
            }
            if let Some(info) = ioctl_absinfo(raw, axis) {
                axis_range[slot] = Some((info.minimum, info.maximum));
            }
        }

        let slot = path
            .file_name()
            .and_then(|n| n.to_str())
            .and_then(|n| n.strip_prefix("event"))
            .and_then(|n| n.parse::<u64>().ok())
            .unwrap_or(0);

        Ok(Some(AdoptedPad {
            fd,
            path: path.to_path_buf(),
            label: if name.is_empty() {
                path.display().to_string()
            } else {
                name
            },
            slot,
            ids: ioctl_input_id(raw).unwrap_or_default(),
            // `ABS_Z`/`ABS_RZ` present means analogue triggers.
            digital_triggers: axis_range[4].is_none() && axis_range[5].is_none(),
            digital_dpad: !has_bit(&axes, abs::HAT0X) && has_bit(&keys, 0x220),
            axis_range,
        }))
    }

    /// Folds everything readable on the adopted pad into [`Self::state`].
    /// Returns false when the pad went away.
    fn read_pending(&mut self) -> bool {
        let Some(pad) = self.pad.as_ref() else {
            return false;
        };
        let raw = pad.fd.as_raw_fd();
        let mut buffer = [RawEvent::default(); 32];
        let mut folded = 0usize;

        while folded < MAX_EVENTS_PER_POLL {
            // SAFETY: `raw` is the adopted pad's live descriptor (the `OwnedFd`
            // outlives this call), and the destination is a local array of
            // `RawEvent`, which is `#[repr(C)]` and plain-old-data, so any byte
            // pattern the kernel writes is a valid value. The length is that
            // array's own size in bytes, so the kernel cannot write past it.
            let read = unsafe {
                libc::read(
                    raw,
                    buffer.as_mut_ptr().cast::<libc::c_void>(),
                    std::mem::size_of_val(&buffer),
                )
            };
            if read < 0 {
                // SAFETY: reading the thread-local `errno` after a failed libc
                // call, which is the documented way to learn why.
                let errno = std::io::Error::last_os_error();
                return match errno.raw_os_error() {
                    Some(libc::EAGAIN) | Some(libc::EINTR) => true,
                    _ => {
                        tracing::debug!(error = %errno, "host gamepad read failed");
                        false
                    }
                };
            }
            if read == 0 {
                return false;
            }
            let count = (read as usize) / std::mem::size_of::<RawEvent>();
            if count == 0 {
                // A short read of less than one record: nothing to fold, and
                // looping would spin.
                return true;
            }
            for event in &buffer[..count] {
                let event = *event;
                fold(&mut self.state, pad, event);
            }
            folded += count;
            if count < buffer.len() {
                return true;
            }
        }
        true
    }
}

impl GamepadSource for EvdevSource {
    fn name(&self) -> &'static str {
        "evdev"
    }

    fn poll(&mut self, timeout: Duration) -> Poll {
        self.scan();
        let Some(pad) = self.pad.as_ref() else {
            // No controller: still pace the loop, and let the scan run again
            // on the next tick.
            std::thread::sleep(timeout);
            return Poll::Disconnected;
        };

        let mut fds = [libc::pollfd {
            fd: pad.fd.as_raw_fd(),
            events: libc::POLLIN,
            revents: 0,
        }];
        let millis = i32::try_from(timeout.as_millis()).unwrap_or(i32::MAX);
        // SAFETY: `fds` is one initialised `pollfd` in local storage and the
        // count matches its length; the descriptor is the adopted pad's, alive
        // for the whole call. `poll` writes only `revents`.
        let ready = unsafe { libc::poll(fds.as_mut_ptr(), 1, millis) };
        if ready < 0 {
            let error = std::io::Error::last_os_error();
            if error.raw_os_error() != Some(libc::EINTR) {
                tracing::debug!(%error, "poll on the host gamepad failed");
                self.release("poll failed");
                return Poll::Disconnected;
            }
        } else if ready > 0 {
            // POLLERR/POLLHUP/POLLNVAL is an unplug, not a readable event.
            if fds[0].revents & (libc::POLLERR | libc::POLLHUP | libc::POLLNVAL) != 0 {
                self.release("device hung up");
                return Poll::Disconnected;
            }
            if fds[0].revents & libc::POLLIN != 0 && !self.read_pending() {
                self.release("device disappeared");
                return Poll::Disconnected;
            }
        }

        // Re-borrow: `read_pending` needed `&mut self`.
        match self.pad.as_ref() {
            Some(pad) => Poll::Connected {
                id: PadId {
                    label: pad.label.clone(),
                    slot: pad.slot,
                },
                state: self.state,
            },
            None => Poll::Disconnected,
        }
    }
}

/// Folds one host `input_event` into the pad state.
///
/// Split out of the source so it can be unit-tested without a device: the
/// mapping decisions (which axis is a trigger, which polarity a hat has, how a
/// digital D-pad becomes a hat) are the part that is easy to get wrong.
fn fold(state: &mut PadState, pad: &AdoptedPad, event: RawEvent) {
    match event.event_type {
        ev::KEY => {
            let pressed = event.value != 0;
            if state.set_button_by_code(event.code, pressed) {
                return;
            }
            match event.code {
                // Digital triggers: all or nothing, which is what the pad has.
                0x138 if pad.digital_triggers => state.left_trigger = if pressed { 255 } else { 0 },
                0x139 if pad.digital_triggers => {
                    state.right_trigger = if pressed { 255 } else { 0 }
                }
                // BTN_DPAD_UP/DOWN/LEFT/RIGHT → the hat the guest advertises.
                // A press wins over the opposite direction being stuck down.
                0x220 if pad.digital_dpad => state.hat.1 = if pressed { -1 } else { 0 },
                0x221 if pad.digital_dpad => state.hat.1 = if pressed { 1 } else { 0 },
                0x222 if pad.digital_dpad => state.hat.0 = if pressed { -1 } else { 0 },
                0x223 if pad.digital_dpad => state.hat.0 = if pressed { 1 } else { 0 },
                _ => {}
            }
        }
        ev::ABS => match event.code {
            abs::X => state.left_stick.0 = scaled_stick(pad, 0, event.value),
            abs::Y => state.left_stick.1 = scaled_stick(pad, 1, event.value),
            abs::RX => state.right_stick.0 = scaled_stick(pad, 2, event.value),
            abs::RY => state.right_stick.1 = scaled_stick(pad, 3, event.value),
            abs::Z => state.left_trigger = scaled_trigger(pad, 4, event.value),
            abs::RZ => state.right_trigger = scaled_trigger(pad, 5, event.value),
            abs::HAT0X => state.hat.0 = to_hat(event.value),
            abs::HAT0Y => state.hat.1 = to_hat(event.value),
            _ => {}
        },
        // EV_SYN, EV_MSC (scancodes) and EV_FF status are all uninteresting:
        // the state is complete after every single event, not only at SYN.
        _ => {}
    }
}

/// Host stick reading → guest stick reading, through the host's own range.
fn scaled_stick(pad: &AdoptedPad, slot: usize, value: i32) -> i16 {
    match pad.axis_range[slot] {
        Some(range) => to_stick(value, range),
        // An axis with no `EVIOCGABS` is one the pad said it does not have, so
        // a report for it is a lie: ignore the magnitude, keep the sign.
        None => 0,
    }
}

/// Host trigger reading → guest `0..=255`.
fn scaled_trigger(pad: &AdoptedPad, slot: usize, value: i32) -> u8 {
    match pad.axis_range[slot] {
        Some(range) => to_trigger(value, range),
        None => 0,
    }
}

/// The pad's state at the moment it is adopted, from `EVIOCGKEY` and the
/// `value` field `EVIOCGABS` already returns — so a controller plugged in with
/// the stick pushed over does not jump when it is first moved.
fn seed_state(pad: &AdoptedPad) -> PadState {
    let mut state = PadState::NEUTRAL;
    let raw = pad.fd.as_raw_fd();
    if let Some(keys) = ioctl_bytes(raw, eviocgkey(KEY_BITMAP_BYTES), KEY_BITMAP_BYTES) {
        for code in btn::GAMEPAD {
            state.set_button_by_code(code, has_bit(&keys, code));
        }
    }
    for (slot, &axis) in ANALOGUE_AXES.iter().enumerate() {
        if pad.axis_range[slot].is_none() {
            continue;
        }
        let Some(info) = ioctl_absinfo(raw, axis) else {
            continue;
        };
        match slot {
            0 => state.left_stick.0 = scaled_stick(pad, 0, info.value),
            1 => state.left_stick.1 = scaled_stick(pad, 1, info.value),
            2 => state.right_stick.0 = scaled_stick(pad, 2, info.value),
            3 => state.right_stick.1 = scaled_stick(pad, 3, info.value),
            4 => state.left_trigger = scaled_trigger(pad, 4, info.value),
            _ => state.right_trigger = scaled_trigger(pad, 5, info.value),
        }
    }
    for (axis, component) in [(abs::HAT0X, 0usize), (abs::HAT0Y, 1)] {
        if let Some(info) = ioctl_absinfo(raw, axis) {
            let value = to_hat(info.value);
            if component == 0 {
                state.hat.0 = value;
            } else {
                state.hat.1 = value;
            }
        }
    }
    state
}

// -------------------------------------------------------------- libc glue

/// `open(path, O_RDONLY | O_NONBLOCK | O_CLOEXEC)`.
///
/// Read-only: nothing here ever writes to a host input device, so the
/// descriptor cannot be used to inject input into the host session.
fn open_read_nonblock(path: &Path) -> std::io::Result<OwnedFd> {
    let c_path = CString::new(path.as_os_str().as_encoded_bytes())
        .map_err(|_| std::io::Error::from(std::io::ErrorKind::InvalidInput))?;
    // SAFETY: `c_path` is a NUL-terminated C string that outlives the call, and
    // the flags are a valid combination for `open`. The returned descriptor is
    // handed straight to `OwnedFd`, which owns the close.
    let fd = unsafe {
        libc::open(
            c_path.as_ptr(),
            libc::O_RDONLY | libc::O_NONBLOCK | libc::O_CLOEXEC,
        )
    };
    if fd < 0 {
        return Err(std::io::Error::last_os_error());
    }
    // SAFETY: `fd` is a fresh, valid, owned descriptor from `open` above, and
    // nothing else holds it.
    Ok(unsafe { OwnedFd::from_raw_fd(fd as RawFd) })
}

/// Runs one `EVIOC*` read ioctl into a byte buffer, returning what it filled.
fn ioctl_bytes(fd: RawFd, request: libc::Ioctl, len: usize) -> Option<Vec<u8>> {
    let mut buffer = vec![0u8; len];
    // SAFETY: `fd` is a live descriptor owned by the caller for the duration of
    // this call, `request` is an `_IOC_READ` request whose encoded size is
    // exactly `len` (built by `evioc_read` from the same number), and `buffer`
    // is a `len`-byte allocation — so the kernel writes at most what it owns.
    let filled = unsafe { libc::ioctl(fd, request, buffer.as_mut_ptr()) };
    if filled < 0 {
        return None;
    }
    buffer.truncate((filled as usize).min(len));
    Some(buffer)
}

/// `EVIOCGID` into an [`InputId`].
fn ioctl_input_id(fd: RawFd) -> Option<InputId> {
    let mut id = InputId::default();
    // SAFETY: `fd` is live for the call; the request is `EVIOCGID`, whose
    // encoded size is `size_of::<InputId>()` (the same expression builds
    // both), and the destination is one such struct in local storage.
    // `InputId` is `#[repr(C)]` plain-old-data, so any bytes the kernel writes
    // are valid.
    let result = unsafe { libc::ioctl(fd, eviocgid(), std::ptr::addr_of_mut!(id)) };
    (result >= 0).then_some(id)
}

/// `EVIOCGABS(axis)` into a [`AbsInfo`].
fn ioctl_absinfo(fd: RawFd, axis: u16) -> Option<AbsInfo> {
    let mut info = AbsInfo::default();
    // SAFETY: `fd` is live for the call; the request is `EVIOCGABS(axis)`,
    // whose encoded size is `size_of::<AbsInfo>()` (the same expression builds
    // both), and the destination is one such struct in local storage. `AbsInfo`
    // is `#[repr(C)]` plain-old-data, so any bytes the kernel writes are valid.
    let result = unsafe { libc::ioctl(fd, eviocgabs(axis), std::ptr::addr_of_mut!(info)) };
    if result < 0 {
        None
    } else {
        Some(info)
    }
}

/// Whether this host can do evdev capture at all — the check behind an
/// explicit `backend = "evdev"`.
///
/// Deliberately *not* "is a controller plugged in": that is what hotplug is
/// for, and a run must not fail because the pad is on the desk rather than in
/// the USB port.
pub fn probe() -> Result<(), String> {
    match std::fs::read_dir(INPUT_DIR) {
        Ok(_) => Ok(()),
        Err(error) => Err(format!(
            "{INPUT_DIR} is not readable ({error}); a controller here would never be found"
        )),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A pad shaped like an Xbox controller on the host: analogue triggers
    /// 0..255, hat D-pad, full-range sticks.
    fn xbox_like() -> AdoptedPad {
        AdoptedPad {
            // /dev/null is a real descriptor that is never read here; the
            // mapping functions only ever look at `axis_range` and the flags.
            fd: open_read_nonblock(Path::new("/dev/null")).expect("/dev/null opens"),
            path: PathBuf::from("/dev/input/event7"),
            label: "Test Pad".into(),
            slot: 7,
            ids: InputId::default(),
            axis_range: [
                Some((-32768, 32767)),
                Some((-32768, 32767)),
                Some((-32768, 32767)),
                Some((-32768, 32767)),
                Some((0, 255)),
                Some((0, 255)),
            ],
            digital_triggers: false,
            digital_dpad: false,
        }
    }

    fn key(code: u16, value: i32) -> RawEvent {
        RawEvent {
            event_type: ev::KEY,
            code,
            value,
            ..RawEvent::default()
        }
    }

    fn axis(code: u16, value: i32) -> RawEvent {
        RawEvent {
            event_type: ev::ABS,
            code,
            value,
            ..RawEvent::default()
        }
    }

    #[test]
    fn the_ioctl_numbers_match_linux_input_h() {
        // Spelled out against the values `evtest` and the kernel headers use,
        // because a wrong `_IOC` encoding fails silently: the ioctl returns
        // -EINVAL, the capability bitmap comes back empty and every pad on the
        // machine looks like "not a gamepad".
        assert_eq!(eviocgid(), 0x8008_4502u32 as libc::Ioctl);
        assert_eq!(eviocgname(128), 0x8080_4506u32 as libc::Ioctl);
        assert_eq!(eviocgkey(96), 0x8060_4518u32 as libc::Ioctl);
        // EVIOCGBIT(EV_KEY = 1, 96) => nr 0x21.
        assert_eq!(eviocgbit(ev::KEY, 96), 0x8060_4521u32 as libc::Ioctl);
        // EVIOCGBIT(EV_ABS = 3, 8) => nr 0x23.
        assert_eq!(eviocgbit(ev::ABS, 8), 0x8008_4523u32 as libc::Ioctl);
        // EVIOCGABS(ABS_X = 0) => nr 0x40, size 24.
        assert_eq!(eviocgabs(abs::X), 0x8018_4540u32 as libc::Ioctl);
        assert_eq!(eviocgabs(abs::RZ), 0x8018_4545u32 as libc::Ioctl);
    }

    #[test]
    fn the_structs_are_the_sizes_the_kernel_writes() {
        assert_eq!(std::mem::size_of::<InputId>(), 8);
        assert_eq!(std::mem::size_of::<AbsInfo>(), 24);
        assert_eq!(std::mem::size_of::<RawEvent>(), 24);
    }

    #[test]
    fn bitmap_bits_are_read_the_way_evdev_writes_them() {
        let mut bitmap = vec![0u8; KEY_BITMAP_BYTES];
        bitmap[38] = 0b0000_0001; // BTN_SOUTH = 0x130 = 304 => byte 38 bit 0
        assert!(has_bit(&bitmap, btn::SOUTH));
        assert!(!has_bit(&bitmap, btn::EAST));
        // Past the end of the buffer is "absent", never a panic.
        assert!(!has_bit(&bitmap, u16::MAX));
        assert!(!has_bit(&[], btn::SOUTH));
    }

    #[test]
    fn an_xbox_shaped_host_pad_maps_one_to_one() {
        let pad = xbox_like();
        let mut state = PadState::NEUTRAL;
        for code in btn::GAMEPAD {
            fold(&mut state, &pad, key(code, 1));
        }
        assert_eq!(state.buttons, [true; super::super::BUTTON_COUNT]);

        fold(&mut state, &pad, axis(abs::X, -32768));
        fold(&mut state, &pad, axis(abs::Y, 32767));
        fold(&mut state, &pad, axis(abs::RX, 0));
        fold(&mut state, &pad, axis(abs::Z, 255));
        fold(&mut state, &pad, axis(abs::RZ, 128));
        fold(&mut state, &pad, axis(abs::HAT0X, -1));
        fold(&mut state, &pad, axis(abs::HAT0Y, 1));
        assert_eq!(state.left_stick, (-32768, 32767));
        assert_eq!(state.right_stick, (0, 0));
        assert_eq!((state.left_trigger, state.right_trigger), (255, 128));
        assert_eq!(state.hat, (-1, 1));
    }

    #[test]
    fn a_pad_with_a_wider_trigger_range_is_rescaled_not_truncated() {
        // An Xbox One pad reports 0..1023. Forwarding that raw would make a
        // quarter-pull read as a full pull in the guest.
        let mut pad = xbox_like();
        pad.axis_range[4] = Some((0, 1023));
        pad.axis_range[5] = Some((0, 1023));
        let mut state = PadState::NEUTRAL;
        fold(&mut state, &pad, axis(abs::Z, 1023));
        fold(&mut state, &pad, axis(abs::RZ, 512));
        assert_eq!(state.left_trigger, 255);
        assert_eq!(state.right_trigger, 127);
    }

    #[test]
    fn a_pad_with_an_unsigned_stick_range_still_centres() {
        let mut pad = xbox_like();
        pad.axis_range[0] = Some((0, 255));
        let mut state = PadState::NEUTRAL;
        fold(&mut state, &pad, axis(abs::X, 128));
        assert_eq!(state.left_stick.0, 128);
        fold(&mut state, &pad, axis(abs::X, 0));
        assert_eq!(state.left_stick.0, -32768);
        fold(&mut state, &pad, axis(abs::X, 255));
        assert_eq!(state.left_stick.0, 32767);
    }

    #[test]
    fn digital_triggers_and_a_digital_dpad_become_the_analogue_shape() {
        let mut pad = xbox_like();
        pad.digital_triggers = true;
        pad.digital_dpad = true;
        pad.axis_range[4] = None;
        pad.axis_range[5] = None;
        let mut state = PadState::NEUTRAL;
        fold(&mut state, &pad, key(0x138, 1)); // BTN_TL2
        fold(&mut state, &pad, key(0x223, 1)); // BTN_DPAD_RIGHT
        fold(&mut state, &pad, key(0x220, 1)); // BTN_DPAD_UP
        assert_eq!(state.left_trigger, 255);
        assert_eq!(state.right_trigger, 0);
        assert_eq!(state.hat, (1, -1));
        fold(&mut state, &pad, key(0x138, 0));
        fold(&mut state, &pad, key(0x220, 0));
        assert_eq!(state.left_trigger, 0);
        assert_eq!(state.hat, (1, 0));
    }

    #[test]
    fn events_the_pad_never_advertised_change_nothing() {
        let pad = xbox_like();
        let mut state = PadState::NEUTRAL;
        for event in [
            key(30, 1),      // KEY_A
            key(0x110, 1),   // BTN_LEFT
            key(0x220, 1),   // BTN_DPAD_UP on a hat pad
            key(0x138, 1),   // BTN_TL2 on an analogue-trigger pad
            axis(0x08, 500), // ABS_WHEEL
            axis(0x28, 1),   // ABS_MISC
            RawEvent {
                event_type: ev::SYN,
                ..RawEvent::default()
            },
            RawEvent {
                event_type: 0x04, // EV_MSC
                code: 4,
                value: 0x9_0001,
                ..RawEvent::default()
            },
        ] {
            fold(&mut state, &pad, event);
        }
        assert_eq!(state, PadState::NEUTRAL);
    }

    #[test]
    fn an_axis_the_pad_denied_having_reports_as_centred() {
        let mut pad = xbox_like();
        pad.axis_range[2] = None; // no ABS_RX
        let mut state = PadState::NEUTRAL;
        fold(&mut state, &pad, axis(abs::RX, 32767));
        assert_eq!(state.right_stick.0, 0);
    }

    /// A scratch directory under `/tmp`, removed on drop.
    struct ScratchDir(PathBuf);

    impl ScratchDir {
        fn new(tag: &str) -> Self {
            let path = std::env::temp_dir().join(format!(
                "entangled-evdev-{tag}-{}-{:?}",
                std::process::id(),
                std::thread::current().id()
            ));
            let _ = std::fs::remove_dir_all(&path);
            std::fs::create_dir_all(&path).expect("scratch directory");
            Self(path)
        }
    }

    impl Drop for ScratchDir {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.0);
        }
    }

    #[test]
    fn one_scan_looks_at_a_bounded_number_of_nodes_and_at_the_lowest_ones() {
        let dir = ScratchDir::new("scan");
        // Twice the bound, plus the things `/dev/input` really contains
        // alongside the event nodes.
        for n in 0..MAX_SCANNED_DEVICES * 2 {
            std::fs::write(dir.0.join(format!("event{n}")), b"").expect("node");
        }
        for name in ["mice", "mouse0", "js0", "by-id", "by-path"] {
            std::fs::write(dir.0.join(name), b"").expect("node");
        }

        let nodes = candidate_nodes(&dir.0);
        assert_eq!(
            nodes.len(),
            MAX_SCANNED_DEVICES,
            "a scan must be bounded however many nodes exist"
        );
        // Numerically lowest first, and nothing that is not an event node.
        let names: Vec<String> = nodes
            .iter()
            .map(|p| p.file_name().unwrap().to_string_lossy().into_owned())
            .collect();
        assert_eq!(names[0], "event0");
        assert_eq!(names[1], "event1");
        assert_eq!(names[9], "event9");
        assert_eq!(names[10], "event10", "sorted numerically, not lexically");
        assert!(names.iter().all(|n| n.starts_with("event")));

        // A directory that is not there is not a panic.
        assert!(candidate_nodes(&dir.0.join("missing")).is_empty());
    }

    #[test]
    fn one_poll_folds_a_bounded_number_of_events_and_leaves_the_rest() {
        // A pad reporting at a plausible rate produces eight events per tick;
        // this one produces far more than the cap in one go, which is what a
        // device gone mad looks like. The loop must stop, and the leftovers
        // must still be there for the next tick rather than being dropped.
        let mut fds = [0 as RawFd; 2];
        // SAFETY: `pipe2` writes two descriptors into a two-element array of
        // `c_int`, which is exactly what `fds` is; the flags are valid.
        let made = unsafe { libc::pipe2(fds.as_mut_ptr(), libc::O_NONBLOCK) };
        assert_eq!(made, 0, "pipe2: {}", std::io::Error::last_os_error());
        // SAFETY: both descriptors were just created by `pipe2` and are owned
        // by nothing else, so taking ownership of them here is sound.
        let (read_end, write_end) =
            unsafe { (OwnedFd::from_raw_fd(fds[0]), OwnedFd::from_raw_fd(fds[1])) };

        let events = MAX_EVENTS_PER_POLL + 40;
        let mut bytes = Vec::with_capacity(events * std::mem::size_of::<RawEvent>());
        for n in 0..events {
            let event = key(btn::SOUTH, i32::from(n % 2 == 0));
            // SAFETY: `RawEvent` is `#[repr(C)]` plain-old-data with no
            // padding-sensitive invariants, so reading it as its own bytes is
            // sound and is exactly the encoding the kernel uses.
            let raw: &[u8] = unsafe {
                std::slice::from_raw_parts(
                    std::ptr::addr_of!(event).cast::<u8>(),
                    std::mem::size_of::<RawEvent>(),
                )
            };
            bytes.extend_from_slice(raw);
        }
        // A pipe holds 64 KiB by default; the payload has to fit or the
        // non-blocking write below would short-write and test nothing.
        assert!(bytes.len() < 64 * 1024, "payload must fit one pipe buffer");
        // SAFETY: `write_end` is live and `bytes` is a valid slice of its own
        // length.
        let written = unsafe {
            libc::write(
                write_end.as_raw_fd(),
                bytes.as_ptr().cast::<libc::c_void>(),
                bytes.len(),
            )
        };
        assert_eq!(written, bytes.len() as isize, "the whole payload is queued");

        let mut pad = xbox_like();
        pad.fd = read_end;
        let mut source = EvdevSource::new();
        source.pad = Some(pad);
        assert!(source.read_pending(), "a full pipe is not a disconnect");

        // The cap held: there is still unread data, so the loop stopped on the
        // bound rather than on the pipe running dry.
        let leftover = source.pad.as_ref().expect("still adopted").fd.as_raw_fd();
        let mut poll_fd = [libc::pollfd {
            fd: leftover,
            events: libc::POLLIN,
            revents: 0,
        }];
        // SAFETY: one initialised `pollfd` in local storage, a matching count,
        // and a descriptor owned by the adopted pad for the whole call.
        let ready = unsafe { libc::poll(poll_fd.as_mut_ptr(), 1, 0) };
        assert_eq!(ready, 1, "the events past the cap must still be readable");

        // …and the next tick drains them, so nothing was lost.
        assert!(source.read_pending());
        poll_fd[0].revents = 0;
        // SAFETY: as above — the same one-element array and the same live
        // descriptor, which the adopted pad still owns.
        let ready = unsafe { libc::poll(poll_fd.as_mut_ptr(), 1, 0) };
        assert_eq!(ready, 0, "the second tick reads the remainder");
    }

    #[test]
    fn probing_this_host_says_something_definite() {
        // On a machine with /dev/input this succeeds; in a container without
        // it, it must fail with a message rather than panicking. Both are a
        // pass — what is tested is that it always answers.
        let answer = probe();
        assert!(answer.is_ok() || answer.unwrap_err().contains("/dev/input"));
    }

    #[test]
    fn a_source_with_no_controller_reports_disconnected_and_paces_itself() {
        // Runs on any Linux host, with or without /dev/input, with or without
        // a pad: what it must never do is claim a controller it did not adopt.
        let mut source = EvdevSource::new();
        assert_eq!(source.name(), "evdev");
        let answer = source.poll(Duration::from_millis(5));
        match answer {
            Poll::Disconnected => {}
            // A developer machine with a real pad plugged in: then the label
            // must at least be non-empty and the state in range.
            Poll::Connected { id, state } => {
                assert!(!id.label.is_empty());
                // A hat is a sign: whatever range the host pad reported it in,
                // `to_hat` must already have collapsed it to -1/0/1.
                assert!((-1..=1).contains(&state.hat.0));
                assert!((-1..=1).contains(&state.hat.1));
                // And every event that state could produce is one the guest
                // device advertises — the same invariant the pump relies on.
                for event in state.delta(&PadState::NEUTRAL) {
                    assert!(
                        crate::config::Profile::Gamepad.accepts(event),
                        "{event:?} is not on the gamepad profile"
                    );
                }
            }
        }
    }
}
