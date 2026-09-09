//! The virtio-input device (backlog MVP-901, 903, 904, 905).
//!
//! One [`InputDevice`] is one guest input device, its personality chosen by a
//! [`Profile`]. Two virtqueues, as the spec (1.2, section 5.8.2) defines them:
//!
//! | Index | Queue | Direction | Contents |
//! |---|---|---|---|
//! | 0 | `eventq` | device → driver | one `virtio_input_event` per chain |
//! | 1 | `statusq` | driver → device | `EV_LED`/`EV_REP` updates from the guest |
//!
//! # Event path
//!
//! The producer is the host: `display`'s `InputQueue` hands
//! `SYN_REPORT`-terminated batches to [`InputHandle::push`], which is callable
//! from any thread. The queue state lives behind a mutex in a shared inner
//! object rather than inside the device struct, because the two sides arrive
//! from different directions: the guest kick comes through
//! `VirtioDevice::notify` on a vCPU thread (the transport owns the device by
//! then), while host events come from the window event loop. Both paths take
//! the same lock and run the same drain loop, so there is exactly one place
//! that writes the event queue.
//!
//! Delivery rules:
//!
//! * events pushed before the driver sets `DRIVER_OK` are dropped — there is no
//!   queue to put them in, and a keystroke from before the guest booted is not
//!   worth replaying;
//! * events that arrive while the driver has no free `eventq` buffers are
//!   buffered, bounded by [`MAX_PENDING_EVENTS`], dropping the *oldest* beyond
//!   that (newer input is the input the user cares about) and counting the loss
//!   in [`EventStats`];
//! * the next kick on `eventq` — or the next push — drains the buffer;
//! * a reset drops everything buffered and deactivates the device.
//!
//! # Untrusted guest
//!
//! Every `eventq` buffer is guest-supplied. A chain that is looped, indexed
//! past the ring, device-readable (wrong direction for this queue), shorter
//! than one event or pointing outside guest RAM is returned unused with length
//! 0 and logged as a [`BufferError`]; the pending event stays queued for the
//! next usable buffer. None of that touches the device's health — a broken
//! driver loses its own input, it cannot take the VM down.

use std::collections::VecDeque;
use std::sync::{Arc, Mutex, MutexGuard};

use thiserror::Error;
use virtio_core::chain::{self, ChainError};
use virtio_core::device::{DeviceError, DeviceResources, DeviceType, VirtioDevice};
use virtio_core::interrupt::Interrupt;
use virtio_core::{GuestMem, VIRTIO_F_VERSION_1};
use virtio_queue::{Queue, QueueT};
use vm_memory::{Bytes, GuestAddress};

use crate::config::{self, Profile};
use crate::gamepad::{GamepadCapture, SourceFactory};
use crate::InputEvent;

/// Number of virtqueues: `eventq` and `statusq`.
pub const NUM_QUEUES: usize = 2;
/// Queue index of `eventq` (device → driver).
pub const EVENT_QUEUE: u16 = 0;
/// Queue index of `statusq` (driver → device).
pub const STATUS_QUEUE: u16 = 1;

/// Queue size both queues advertise. Human input is slow and every chain
/// carries exactly one 8-byte event, so 64 outstanding buffers is already far
/// more than a frame's worth of typing.
const QUEUE_SIZE: u16 = 64;
static QUEUE_MAX_SIZES: [u16; NUM_QUEUES] = [QUEUE_SIZE; NUM_QUEUES];

/// How many events are buffered while the guest is not refilling `eventq`.
///
/// A guest that stops draining must not grow host memory without bound. 1024
/// events is roughly 200 keystrokes or 500 pointer moves — several seconds of
/// frantic input — after which the oldest event is dropped.
pub const MAX_PENDING_EVENTS: usize = 1024;

/// Hard bound on how many chains one drain may process, so a guest refilling
/// the ring from another vCPU cannot pin this thread forever. Leftovers are
/// picked up by the next kick or push.
const CHAINS_PER_DRAIN: usize = 4 * QUEUE_SIZE as usize;

/// How many events one status-queue chain is inspected for. The guest can make
/// a chain arbitrarily long; we only log them, so a small bound is plenty.
const STATUS_EVENTS_PER_CHAIN: usize = 16;

/// Why one guest buffer could not carry an event.
///
/// All of these are guest-caused, so none of them is a device failure: the
/// chain goes back to the driver with length 0 and the event waits for a usable
/// buffer.
#[derive(Debug, Error, PartialEq, Eq)]
pub enum BufferError {
    #[error("descriptor chain rejected: {0}")]
    Chain(#[from] ChainError),

    #[error("event queue chain has {count} device-readable descriptor(s); eventq is write-only")]
    WrongDirection { count: usize },

    #[error("event queue chain has no device-writable descriptor")]
    NotWritable,

    #[error(
        "device-writable buffer is {len} bytes, one virtio_input_event needs {}",
        InputEvent::WIRE_SIZE
    )]
    TooSmall { len: u32 },

    #[error("event buffer at {addr:#x} is not writable guest memory: {reason}")]
    Unwritable { addr: u64, reason: String },
}

/// Counters for diagnostics (`entangled doctor`, the periodic MVP-708 log record).
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct EventStats {
    /// Events accepted into the pending ring.
    pub queued: u64,
    /// Events written into a guest buffer.
    pub delivered: u64,
    /// Events dropped because the driver had not reached `DRIVER_OK`.
    pub dropped_inactive: u64,
    /// Events dropped because the pending ring was full.
    pub dropped_overflow: u64,
    /// Events dropped by a device reset.
    pub dropped_reset: u64,
    /// Guest buffers refused (see [`BufferError`]).
    pub rejected_buffers: u64,
    /// Status-queue chains drained and acknowledged.
    pub status_chains: u64,
}

/// Resources handed over at `DRIVER_OK`.
struct Active {
    mem: Arc<GuestMem>,
    interrupt: Arc<dyn Interrupt>,
    eventq: Queue,
    statusq: Queue,
}

#[derive(Default)]
struct State {
    /// `None` until `activate()`, `None` again after `reset()`.
    active: Option<Active>,
    pending: VecDeque<InputEvent>,
    stats: EventStats,
}

/// Everything both the guest side and the host side reach.
struct Shared {
    profile: Profile,
    state: Mutex<State>,
}

/// Locks the shared state, recovering from poisoning instead of panicking.
///
/// Every critical section here is a bounded copy or queue operation, so a
/// poisoned lock means an unrelated thread died while holding it: the data is
/// still structurally valid and the VM must keep running (`unwrap()` on a
/// runtime path is a workspace hard rule violation).
fn lock(state: &Mutex<State>) -> MutexGuard<'_, State> {
    match state.lock() {
        Ok(guard) => guard,
        Err(poisoned) => {
            tracing::warn!("virtio-input state mutex was poisoned; recovering");
            poisoned.into_inner()
        }
    }
}

impl Shared {
    /// Buffers `events` and delivers as many as the guest has room for.
    fn push(&self, events: &[InputEvent]) -> Result<usize, DeviceError> {
        if events.is_empty() {
            return Ok(0);
        }
        let mut state = lock(&self.state);
        if state.active.is_none() {
            state.stats.dropped_inactive = state
                .stats
                .dropped_inactive
                .saturating_add(events.len() as u64);
            tracing::debug!(
                device = self.profile.name(),
                events = events.len(),
                "dropping input events: driver has not set DRIVER_OK"
            );
            return Ok(0);
        }

        let mut overflowed = 0usize;
        for &event in events {
            if state.pending.len() >= MAX_PENDING_EVENTS {
                let _ = state.pending.pop_front();
                overflowed += 1;
            }
            state.pending.push_back(event);
            state.stats.queued = state.stats.queued.saturating_add(1);
        }
        if overflowed > 0 {
            state.stats.dropped_overflow = state
                .stats
                .dropped_overflow
                .saturating_add(overflowed as u64);
            tracing::warn!(
                device = self.profile.name(),
                dropped = overflowed,
                total = state.stats.dropped_overflow,
                "guest is not draining its input queue; dropped the oldest events"
            );
        }
        self.drain_events(&mut state)
    }

    /// Writes pending events into whatever `eventq` buffers the driver offered.
    /// Returns how many events reached the guest.
    fn drain_events(&self, state: &mut State) -> Result<usize, DeviceError> {
        let State {
            active,
            pending,
            stats,
        } = state;
        let Some(active) = active.as_mut() else {
            // Not activated: nothing may stay buffered either.
            pending.clear();
            return Ok(0);
        };
        let mem = Arc::clone(&active.mem);
        let desc_table = active.eventq.desc_table();
        let queue_size = active.eventq.size();

        let mut delivered = 0usize;
        let mut chains = 0usize;
        while chains < CHAINS_PER_DRAIN {
            let Some(&event) = pending.front() else {
                break;
            };
            let Some(head) = active
                .eventq
                .pop_descriptor_chain(Arc::clone(&mem))
                .map(|chain| chain.head_index())
            else {
                break;
            };
            let written = match write_event(&mem, desc_table, queue_size, head, event) {
                Ok(()) => {
                    let _ = pending.pop_front();
                    delivered += 1;
                    stats.delivered = stats.delivered.saturating_add(1);
                    // `WIRE_SIZE` is 8; the cast cannot truncate.
                    InputEvent::WIRE_SIZE as u32
                }
                Err(error) => {
                    stats.rejected_buffers = stats.rejected_buffers.saturating_add(1);
                    tracing::warn!(
                        device = self.profile.name(),
                        head,
                        %error,
                        "unusable virtio-input event buffer; returning it unused"
                    );
                    0
                }
            };
            active
                .eventq
                .add_used(mem.as_ref(), head, written)
                .map_err(|e| DeviceError::Queue(e.to_string()))?;
            chains += 1;
        }

        if chains >= CHAINS_PER_DRAIN {
            tracing::warn!(
                device = self.profile.name(),
                chains,
                pending = pending.len(),
                "virtio-input drain budget exhausted; deferring the rest"
            );
        }

        // Signal whenever a buffer went back to the driver, including the ones
        // returned unused — otherwise a driver waiting for its buffer hangs.
        if chains > 0
            && active
                .eventq
                .needs_notification(mem.as_ref())
                .map_err(|e| DeviceError::Queue(e.to_string()))?
        {
            active.interrupt.signal_used_queue(EVENT_QUEUE)?;
        }
        Ok(delivered)
    }

    /// Drains `statusq`: the guest writes `EV_LED`/`EV_REP` updates there. The
    /// MVP has no LEDs and no host-side repeat handling, so every chain is
    /// logged and acknowledged with length 0 (nothing is written back).
    ///
    /// TODO(MVP-9xx): act on `EV_LED` (mirror caps/num/scroll lock into the
    /// host window title or an indicator) and on `EV_REP` (the guest can ask
    /// for a different repeat delay/period; today the guest's own input core
    /// generates repeats, so there is nothing to configure).
    fn drain_status(&self, state: &mut State) -> Result<(), DeviceError> {
        let State { active, stats, .. } = state;
        let Some(active) = active.as_mut() else {
            return Err(DeviceError::NotActivated);
        };
        let mem = Arc::clone(&active.mem);
        let desc_table = active.statusq.desc_table();
        let queue_size = active.statusq.size();

        let mut chains = 0usize;
        while chains < CHAINS_PER_DRAIN {
            let Some(head) = active
                .statusq
                .pop_descriptor_chain(Arc::clone(&mem))
                .map(|chain| chain.head_index())
            else {
                break;
            };
            self.log_status_chain(&mem, desc_table, queue_size, head);
            active
                .statusq
                .add_used(mem.as_ref(), head, 0)
                .map_err(|e| DeviceError::Queue(e.to_string()))?;
            chains += 1;
            stats.status_chains = stats.status_chains.saturating_add(1);
        }

        if chains > 0
            && active
                .statusq
                .needs_notification(mem.as_ref())
                .map_err(|e| DeviceError::Queue(e.to_string()))?
        {
            active.interrupt.signal_used_queue(STATUS_QUEUE)?;
        }
        Ok(())
    }

    /// Reads what the guest put on the status queue, purely for diagnostics.
    /// Every failure is swallowed: a malformed status chain is still acked.
    fn log_status_chain(&self, mem: &GuestMem, desc_table: u64, queue_size: u16, head: u16) {
        let segments = match chain::walk(mem, desc_table, queue_size, head) {
            Ok(segments) => segments,
            Err(error) => {
                tracing::warn!(
                    device = self.profile.name(),
                    head,
                    %error,
                    "malformed virtio-input status chain; acknowledging it unread"
                );
                return;
            }
        };
        let Ok((readable, _writable)) = chain::split_rw(&segments) else {
            tracing::warn!(
                device = self.profile.name(),
                head,
                "virtio-input status chain interleaves buffer directions"
            );
            return;
        };
        let mut seen = 0usize;
        for segment in readable {
            let mut offset = 0u32;
            while seen < STATUS_EVENTS_PER_CHAIN
                && segment.len.saturating_sub(offset) as usize >= InputEvent::WIRE_SIZE
            {
                let mut raw = [0u8; InputEvent::WIRE_SIZE];
                let Some(addr) = segment.addr.checked_add(u64::from(offset)) else {
                    return;
                };
                if mem.read_slice(&mut raw, GuestAddress(addr)).is_err() {
                    tracing::debug!(
                        device = self.profile.name(),
                        head,
                        addr = format_args!("{addr:#x}"),
                        "virtio-input status buffer is not readable guest memory"
                    );
                    return;
                }
                let event = InputEvent::from_le_bytes(raw);
                tracing::debug!(
                    device = self.profile.name(),
                    event_type = event.event_type,
                    code = event.code,
                    value = event.value,
                    "ignoring virtio-input status event (no LED/REP handling yet)"
                );
                seen += 1;
                offset = offset.saturating_add(InputEvent::WIRE_SIZE as u32);
            }
        }
    }
}

/// Writes one event into the first device-writable buffer of `head`'s chain.
///
/// The chain walk is bounded and index-checked by `virtio_core::chain`, and the
/// buffer address — still entirely guest-supplied at this point — is only ever
/// used through `vm-memory`'s checked API, so a buffer outside guest RAM is an
/// error rather than a host memory-safety problem.
fn write_event(
    mem: &GuestMem,
    desc_table: u64,
    queue_size: u16,
    head: u16,
    event: InputEvent,
) -> Result<(), BufferError> {
    let segments = chain::walk(mem, desc_table, queue_size, head)?;
    let (readable, writable) = chain::split_rw(&segments)?;
    if !readable.is_empty() {
        return Err(BufferError::WrongDirection {
            count: readable.len(),
        });
    }
    let Some(segment) = writable.first() else {
        return Err(BufferError::NotWritable);
    };
    if (segment.len as usize) < InputEvent::WIRE_SIZE {
        return Err(BufferError::TooSmall { len: segment.len });
    }
    mem.write_slice(&event.to_le_bytes(), GuestAddress(segment.addr))
        .map_err(|error| BufferError::Unwritable {
            addr: segment.addr,
            reason: error.to_string(),
        })
}

/// The host-side event sink of one [`InputDevice`].
///
/// Cheap to clone, `Send + Sync`, and it stays usable after the device has been
/// handed to a transport — which is the point: `MmioTransport::new` takes
/// ownership of the device, so the integrator keeps a handle instead.
#[derive(Clone)]
pub struct InputHandle {
    shared: Arc<Shared>,
}

impl std::fmt::Debug for InputHandle {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("InputHandle")
            .field("device", &self.shared.profile.name())
            .field("active", &self.is_active())
            .finish_non_exhaustive()
    }
}

impl InputHandle {
    /// Which device this handle feeds.
    pub fn profile(&self) -> Profile {
        self.shared.profile
    }

    /// Queues one batch of events and delivers what fits, returning how many
    /// events reached the guest.
    ///
    /// `events` must be a complete, `SYN_REPORT`-terminated stream — the device
    /// never inserts or reorders anything, so a caller that drops the
    /// terminating `SYN_REPORT` leaves the guest holding an incomplete report.
    /// An `Err` means the *host* side failed (the interrupt line or the used
    /// ring); guest-caused problems are logged and never surface here.
    pub fn push(&self, events: &[InputEvent]) -> Result<usize, DeviceError> {
        self.shared.push(events)
    }

    /// True once the driver has set `DRIVER_OK` and before any reset.
    pub fn is_active(&self) -> bool {
        lock(&self.shared.state).active.is_some()
    }

    /// Events waiting for a guest buffer.
    pub fn pending(&self) -> usize {
        lock(&self.shared.state).pending.len()
    }

    /// Delivery counters.
    pub fn stats(&self) -> EventStats {
        lock(&self.shared.state).stats
    }
}

/// A virtio-input device: keyboard or absolute pointer, per its [`Profile`].
pub struct InputDevice {
    shared: Arc<Shared>,
    features: u64,
    acked_features: u64,
    /// `select`/`subsel` config registers — guest-writable, and the only
    /// mutable config state. Serialised by the transport, which is the sole
    /// caller of `read_config`/`write_config`.
    select: u8,
    subsel: u8,
    /// Host gamepad capture, if this device was built with any (GAME-2104).
    ///
    /// A factory rather than a source: the pump is tied to one *activation*,
    /// so a guest that resets the device gets a fresh source with fresh file
    /// descriptors rather than one that was already half-read.
    capture: Option<SourceFactory>,
    /// The running pump, `Some` only between `activate()` and `reset()`.
    pump: Option<GamepadCapture>,
}

impl std::fmt::Debug for InputDevice {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("InputDevice")
            .field("profile", &self.shared.profile)
            .field("name", &self.shared.profile.name())
            .field("select", &self.select)
            .field("subsel", &self.subsel)
            .finish_non_exhaustive()
    }
}

impl InputDevice {
    /// Builds a device with the given profile.
    pub fn new(profile: Profile) -> Self {
        Self {
            shared: Arc::new(Shared {
                profile,
                state: Mutex::new(State::default()),
            }),
            // No virtio-input feature bits exist beyond the transport-level
            // ones (the spec defines none), so VERSION_1 is the whole set.
            features: VIRTIO_F_VERSION_1,
            acked_features: 0,
            select: config::VIRTIO_INPUT_CFG_UNSET,
            subsel: 0,
            capture: None,
            pump: None,
        }
    }

    /// A keyboard (MVP-901/902).
    pub fn keyboard() -> Self {
        Self::new(Profile::Keyboard)
    }

    /// A tablet-style absolute pointer (MVP-901/903).
    pub fn absolute_pointer() -> Self {
        Self::new(Profile::AbsolutePointer)
    }

    /// A gamepad with no host capture attached (GAME-2104): the guest gets a
    /// pad that enumerates and never moves unless something pushes events into
    /// its [`InputHandle`]. That is what the tests use, and what a headless run
    /// gets. [`crate::gamepad::GamepadCapture`] is the half that makes a real
    /// controller drive it.
    pub fn gamepad() -> Self {
        Self::new(Profile::Gamepad)
    }

    /// A gamepad fed by a real host controller (GAME-2104).
    ///
    /// The capture thread is started when the driver sets `DRIVER_OK` and
    /// stopped by a reset or by dropping the device — the same lifetime
    /// virtio-net's receive worker has, and for the same reason: it is a host
    /// thread that writes guest memory, so it belongs to one activation and
    /// owes the pause gate a wait (ADR-0005).
    ///
    /// ```no_run
    /// use virtio_input::{gamepad, InputDevice};
    ///
    /// let (mechanism, factory) = gamepad::open_source(gamepad::SourceChoice::Auto)
    ///     .expect("auto never fails");
    /// tracing::info!(mechanism, "attaching virtio-input gamepad");
    /// let pad = InputDevice::gamepad_with_capture(factory);
    /// ```
    pub fn gamepad_with_capture(capture: SourceFactory) -> Self {
        let mut device = Self::new(Profile::Gamepad);
        device.capture = Some(capture);
        device
    }

    /// True while a host capture thread is running for this device.
    pub fn is_capturing(&self) -> bool {
        self.pump.is_some()
    }

    /// Stops and joins the capture thread. Idempotent and infallible, so both
    /// `reset()` and `Drop` can call it.
    fn stop_capture(&mut self) {
        if let Some(mut pump) = self.pump.take() {
            pump.stop();
        }
    }

    /// This device's profile.
    pub fn profile(&self) -> Profile {
        self.shared.profile
    }

    /// The guest-visible device name.
    pub fn name(&self) -> &'static str {
        self.shared.profile.name()
    }

    /// A host-side sink for this device. Take it *before* handing the device to
    /// a transport; any number of handles may coexist.
    pub fn handle(&self) -> InputHandle {
        InputHandle {
            shared: Arc::clone(&self.shared),
        }
    }

    /// Convenience wrapper around [`InputHandle::push`] for callers that still
    /// own the device (tests, single-threaded integrations).
    pub fn push_events(&self, events: &[InputEvent]) -> Result<usize, DeviceError> {
        self.shared.push(events)
    }

    /// Delivery counters.
    pub fn stats(&self) -> EventStats {
        lock(&self.shared.state).stats
    }

    /// Events waiting for a guest buffer.
    pub fn pending(&self) -> usize {
        lock(&self.shared.state).pending.len()
    }

    /// The negotiated feature set (0 before `FEATURES_OK`).
    pub fn acked_features(&self) -> u64 {
        self.acked_features
    }

    /// The `(select, subsel)` pair the guest last wrote.
    pub fn selected(&self) -> (u8, u8) {
        (self.select, self.subsel)
    }
}

impl VirtioDevice for InputDevice {
    fn device_type(&self) -> DeviceType {
        DeviceType::Input
    }

    fn queue_max_sizes(&self) -> &[u16] {
        &QUEUE_MAX_SIZES
    }

    fn device_features(&self) -> u64 {
        self.features
    }

    fn ack_features(&mut self, negotiated: u64) -> bool {
        if negotiated & VIRTIO_F_VERSION_1 == 0 {
            return false;
        }
        if negotiated & !self.features != 0 {
            tracing::warn!(
                device = self.shared.profile.name(),
                negotiated = format_args!("{negotiated:#x}"),
                offered = format_args!("{:#x}", self.features),
                "driver accepted features the device never offered"
            );
            return false;
        }
        self.acked_features = negotiated;
        true
    }

    fn read_config(&self, offset: u64, data: &mut [u8]) {
        let selection = config::selection(self.shared.profile, self.select, self.subsel);
        for (i, byte) in data.iter_mut().enumerate() {
            let index = offset.saturating_add(i as u64);
            *byte = match index {
                config::SELECT => self.select,
                config::SUBSEL => self.subsel,
                config::SIZE => selection.size(),
                // `reserved[5]` at 3..8 and everything past the payload read as
                // zero, as does the payload beyond `size`.
                _ => index
                    .checked_sub(config::PAYLOAD)
                    .and_then(|o| usize::try_from(o).ok())
                    .and_then(|o| selection.payload().get(o))
                    .copied()
                    .unwrap_or(0),
            };
        }
    }

    fn write_config(&mut self, offset: u64, data: &[u8]) {
        let mut ignored = 0usize;
        for (i, byte) in data.iter().enumerate() {
            let index = offset.saturating_add(i as u64);
            match index {
                config::SELECT => self.select = *byte,
                config::SUBSEL => self.subsel = *byte,
                // `size`, `reserved` and the payload are read-only.
                _ => ignored += 1,
            }
        }
        if ignored > 0 {
            tracing::debug!(
                device = self.shared.profile.name(),
                offset,
                ignored,
                "ignoring guest write to read-only virtio-input config bytes"
            );
        }
    }

    fn activate(&mut self, resources: DeviceResources) -> Result<(), DeviceError> {
        if resources.queues.len() != NUM_QUEUES {
            return Err(DeviceError::QueueCount {
                expected: NUM_QUEUES,
                actual: resources.queues.len(),
            });
        }
        let mut queues = resources.queues.into_iter();
        // Length checked above, but destructure fallibly all the same: a `let
        // else` here is cheaper than reasoning about it later.
        let (Some(eventq), Some(statusq)) = (queues.next(), queues.next()) else {
            return Err(DeviceError::QueueCount {
                expected: NUM_QUEUES,
                actual: 0,
            });
        };

        // Re-activation without a reset in between must not leak a thread.
        self.stop_capture();

        let mut state = lock(&self.shared.state);
        // A stale buffer from a previous activation is not this driver's input.
        state.pending.clear();
        state.active = Some(Active {
            mem: resources.mem,
            interrupt: resources.interrupt,
            eventq,
            statusq,
        });
        drop(state);

        // Host capture starts only now: before `DRIVER_OK` there is nowhere to
        // put a button press, and `push` would drop it as `dropped_inactive`.
        if let Some(factory) = self.capture.as_ref() {
            match GamepadCapture::start(self.handle(), Arc::clone(&resources.quiesce), factory()) {
                Ok(pump) => self.pump = Some(pump),
                // A capture thread that will not start costs the guest its
                // controller, not its boot: the device still enumerates and
                // still accepts events pushed by anything else.
                Err(error) => tracing::error!(
                    %error,
                    "host gamepad capture is unavailable; the guest pad will not move"
                ),
            }
        }

        tracing::info!(
            device = self.shared.profile.name(),
            profile = ?self.shared.profile,
            "virtio-input ready"
        );
        Ok(())
    }

    fn notify(&mut self, queue_index: u16) -> Result<(), DeviceError> {
        let mut state = lock(&self.shared.state);
        if state.active.is_none() {
            return Err(DeviceError::NotActivated);
        }
        match queue_index {
            EVENT_QUEUE => {
                // Fresh buffers: hand over whatever the host queued meanwhile.
                self.shared.drain_events(&mut state)?;
                Ok(())
            }
            STATUS_QUEUE => self.shared.drain_status(&mut state),
            other => Err(DeviceError::UnknownQueue(other)),
        }
    }

    /// Both queues' positions, `eventq` first (ADR-0006).
    ///
    /// Nothing else needs carrying. The host-side `pending` buffer is
    /// deliberately dropped by a suspend the same way it is by a reset: it
    /// holds keystrokes and pointer motion that happened *before* the guest was
    /// frozen and were never delivered, and replaying a burst of them into a
    /// guest that resumes minutes later would be worse than losing them.
    fn queue_positions(&self) -> Vec<virtio_core::QueuePosition> {
        use virtio_queue::QueueT as _;
        let state = lock(&self.shared.state);
        state
            .active
            .as_ref()
            .map(|active| {
                [&active.eventq, &active.statusq]
                    .into_iter()
                    .map(|queue| virtio_core::QueuePosition {
                        next_avail: queue.next_avail(),
                        next_used: queue.next_used(),
                    })
                    .collect()
            })
            .unwrap_or_default()
    }

    fn reset(&mut self) {
        // Before the lock: `stop_capture` joins a thread that takes it.
        self.stop_capture();
        let mut state = lock(&self.shared.state);
        let dropped = state.pending.len();
        state.pending.clear();
        state.active = None;
        state.stats.dropped_reset = state.stats.dropped_reset.saturating_add(dropped as u64);
        drop(state);
        self.acked_features = 0;
        self.select = config::VIRTIO_INPUT_CFG_UNSET;
        self.subsel = 0;
        if dropped > 0 {
            tracing::debug!(
                device = self.shared.profile.name(),
                dropped,
                "virtio-input reset dropped buffered events"
            );
        }
    }
}

impl Drop for InputDevice {
    /// Guarantees "closing the VM leaves no capture thread behind" even if the
    /// driver never reset the device.
    fn drop(&mut self) {
        self.stop_capture();
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{abs, btn, ev, key, ABS_AXIS_MAX};

    fn read_config(device: &InputDevice, offset: u64, len: usize) -> Vec<u8> {
        let mut data = vec![0xffu8; len];
        device.read_config(offset, &mut data);
        data
    }

    /// The full driver-side probe of one `(select, subsel)` pair.
    fn probe(device: &mut InputDevice, select: u8, subsel: u8) -> Vec<u8> {
        device.write_config(config::SELECT, &[select, subsel]);
        let size = read_config(device, config::SIZE, 1)[0];
        read_config(device, config::PAYLOAD, usize::from(size))
    }

    #[test]
    fn identifies_as_an_input_device_with_two_queues() {
        let device = InputDevice::keyboard();
        assert_eq!(device.device_type(), DeviceType::Input);
        assert_eq!(device.device_type().id(), 18);
        assert_eq!(device.queue_max_sizes(), &[QUEUE_SIZE, QUEUE_SIZE]);
        assert_eq!(device.num_queues(), 2);
        assert_eq!(device.device_features(), VIRTIO_F_VERSION_1);
        assert_eq!(device.name(), "Entangled Keyboard");
        assert_eq!(InputDevice::absolute_pointer().name(), "Entangled Tablet");
    }

    #[test]
    fn queue_sizes_satisfy_the_transport_contract() {
        // MmioTransport rejects non-power-of-two or oversized queues.
        for &size in InputDevice::keyboard().queue_max_sizes() {
            assert!(size > 0 && size.is_power_of_two());
            assert!(size <= virtio_core::MAX_QUEUE_SIZE);
        }
    }

    #[test]
    fn feature_negotiation_requires_version_1_and_nothing_else() {
        let mut device = InputDevice::keyboard();
        assert!(!device.ack_features(0));
        assert!(!device.ack_features(1 << 3));
        assert!(
            !device.ack_features(VIRTIO_F_VERSION_1 | (1 << 5)),
            "features the device never offered must be vetoed"
        );
        assert_eq!(device.acked_features(), 0);
        assert!(device.ack_features(VIRTIO_F_VERSION_1));
        assert_eq!(device.acked_features(), VIRTIO_F_VERSION_1);
    }

    // -------------------------------------------------- config state machine

    #[test]
    fn select_and_subsel_read_back_and_drive_size() {
        let mut device = InputDevice::keyboard();
        // Pristine: UNSET, size 0.
        assert_eq!(device.selected(), (config::VIRTIO_INPUT_CFG_UNSET, 0));
        assert_eq!(read_config(&device, config::SIZE, 1), vec![0]);

        device.write_config(config::SELECT, &[config::VIRTIO_INPUT_CFG_ID_NAME]);
        assert_eq!(device.selected(), (config::VIRTIO_INPUT_CFG_ID_NAME, 0));
        let header = read_config(&device, 0, 3);
        assert_eq!(header, vec![config::VIRTIO_INPUT_CFG_ID_NAME, 0, 18]);

        // A one-byte write to subsel only touches subsel.
        device.write_config(config::SUBSEL, &[0x2a]);
        assert_eq!(device.selected(), (config::VIRTIO_INPUT_CFG_ID_NAME, 0x2a));

        // Both bytes in one two-byte write, as a driver does.
        device.write_config(config::SELECT, &[config::VIRTIO_INPUT_CFG_EV_BITS, 0x01]);
        assert_eq!(
            device.selected(),
            (config::VIRTIO_INPUT_CFG_EV_BITS, ev::KEY as u8)
        );
        assert_eq!(read_config(&device, config::SIZE, 1), vec![45]);
    }

    #[test]
    fn reserved_bytes_and_reads_past_the_payload_are_zero() {
        let mut device = InputDevice::keyboard();
        device.write_config(config::SELECT, &[config::VIRTIO_INPUT_CFG_ID_NAME]);
        // reserved[5] at offsets 3..8.
        assert_eq!(read_config(&device, 3, 5), vec![0u8; 5]);
        // The payload past `size` is zero…
        let tail = read_config(&device, config::PAYLOAD + 18, 16);
        assert_eq!(tail, vec![0u8; 16]);
        // …and so is everything past the config space, at any width.
        for offset in [config::CONFIG_LEN, config::CONFIG_LEN + 0x400, u64::MAX - 8] {
            assert_eq!(read_config(&device, offset, 8), vec![0u8; 8]);
        }
    }

    #[test]
    fn config_reads_work_at_every_access_width_and_offset() {
        let mut device = InputDevice::absolute_pointer();
        device.write_config(
            config::SELECT,
            &[config::VIRTIO_INPUT_CFG_ABS_INFO, abs::Y as u8],
        );
        // One 24-byte read covering header, reserved bytes and the absinfo.
        let whole = read_config(&device, 0, 28);
        assert_eq!(whole[0], config::VIRTIO_INPUT_CFG_ABS_INFO);
        assert_eq!(whole[1], abs::Y as u8);
        assert_eq!(whole[2], 20);
        assert_eq!(&whole[3..8], &[0u8; 5]);
        assert_eq!(
            u32::from_le_bytes([whole[8], whole[9], whole[10], whole[11]]),
            0
        );
        assert_eq!(
            u32::from_le_bytes([whole[12], whole[13], whole[14], whole[15]]),
            ABS_AXIS_MAX
        );
        assert_eq!(&whole[16..28], &[0u8; 12], "fuzz, flat and res are all 0");

        // Byte-at-a-time reads agree with the wide read.
        for (i, expected) in whole.iter().enumerate() {
            let single = read_config(&device, i as u64, 1);
            assert_eq!(single[0], *expected, "offset {i}");
        }
    }

    #[test]
    fn guest_writes_to_read_only_config_bytes_are_ignored() {
        let mut device = InputDevice::keyboard();
        device.write_config(config::SELECT, &[config::VIRTIO_INPUT_CFG_ID_NAME]);
        // Try to forge a size and a payload.
        device.write_config(config::SIZE, &[0xff]);
        device.write_config(config::PAYLOAD, &[0xde; 32]);
        device.write_config(3, &[0xff; 5]);
        device.write_config(config::CONFIG_LEN + 8, &[0xff; 4]);

        assert_eq!(read_config(&device, config::SIZE, 1), vec![18]);
        assert_eq!(
            read_config(&device, config::PAYLOAD, 18),
            b"Entangled Keyboard".to_vec()
        );
        assert_eq!(read_config(&device, 3, 5), vec![0u8; 5]);
    }

    #[test]
    fn keyboard_probe_sequence_matches_the_config_module() {
        let mut device = InputDevice::keyboard();
        assert_eq!(
            probe(&mut device, config::VIRTIO_INPUT_CFG_ID_NAME, 0),
            b"Entangled Keyboard".to_vec()
        );
        assert_eq!(
            probe(&mut device, config::VIRTIO_INPUT_CFG_ID_DEVIDS, 0),
            vec![0x06, 0x00, 0x4d, 0x56, 0x01, 0x00, 0x01, 0x00]
        );
        let keys = probe(&mut device, config::VIRTIO_INPUT_CFG_EV_BITS, ev::KEY as u8);
        assert_eq!(keys.len(), 45);
        assert_eq!(keys[3] & (1 << 6), 1 << 6, "KEY_A = 30 is byte 3 bit 6");
        assert_eq!(keys[44], 0x02, "KEY_SELECT = 353 is byte 44 bit 1");
        assert_eq!(
            probe(&mut device, config::VIRTIO_INPUT_CFG_EV_BITS, ev::REP as u8).len(),
            1
        );
        // Nothing pointer-ish, and no serial.
        for (select, subsel) in [
            (config::VIRTIO_INPUT_CFG_EV_BITS, ev::ABS as u8),
            (config::VIRTIO_INPUT_CFG_EV_BITS, ev::REL as u8),
            (config::VIRTIO_INPUT_CFG_ABS_INFO, abs::X as u8),
            (config::VIRTIO_INPUT_CFG_ID_SERIAL, 0),
            (config::VIRTIO_INPUT_CFG_PROP_BITS, 0),
        ] {
            assert!(probe(&mut device, select, subsel).is_empty());
        }
    }

    #[test]
    fn pointer_probe_sequence_matches_the_config_module() {
        let mut device = InputDevice::absolute_pointer();
        assert_eq!(
            probe(&mut device, config::VIRTIO_INPUT_CFG_ID_NAME, 0),
            b"Entangled Tablet".to_vec()
        );
        assert_eq!(
            probe(&mut device, config::VIRTIO_INPUT_CFG_ID_DEVIDS, 0),
            vec![0x06, 0x00, 0x4d, 0x56, 0x02, 0x00, 0x01, 0x00]
        );
        let buttons = probe(&mut device, config::VIRTIO_INPUT_CFG_EV_BITS, ev::KEY as u8);
        assert_eq!(buttons.len(), 35);
        assert_eq!(buttons[34], 0x1f, "BTN_LEFT = 0x110 is byte 34 bit 0");
        assert!(buttons[..34].iter().all(|&b| b == 0));
        assert_eq!(
            probe(&mut device, config::VIRTIO_INPUT_CFG_EV_BITS, ev::ABS as u8),
            vec![0x03]
        );
        assert_eq!(
            probe(&mut device, config::VIRTIO_INPUT_CFG_EV_BITS, ev::REL as u8),
            vec![0x40, 0x01]
        );
        for axis in [abs::X, abs::Y] {
            let info = probe(&mut device, config::VIRTIO_INPUT_CFG_ABS_INFO, axis as u8);
            assert_eq!(info.len(), 20);
            assert_eq!(
                u32::from_le_bytes([info[4], info[5], info[6], info[7]]),
                ABS_AXIS_MAX
            );
        }
        // No auto-repeat on a pointer.
        assert!(probe(&mut device, config::VIRTIO_INPUT_CFG_EV_BITS, ev::REP as u8).is_empty());
    }

    #[test]
    fn every_selector_a_guest_can_write_is_answered() {
        for mut device in [InputDevice::keyboard(), InputDevice::absolute_pointer()] {
            for select in 0..=u8::MAX {
                for subsel in [0u8, 1, 2, 3, 0x11, 0x14, 0xff] {
                    let payload = probe(&mut device, select, subsel);
                    assert!(payload.len() <= config::PAYLOAD_MAX);
                }
            }
        }
    }

    // ------------------------------------------------------ inactive device

    #[test]
    fn events_pushed_before_driver_ok_are_dropped() {
        let device = InputDevice::keyboard();
        let handle = device.handle();
        assert!(!handle.is_active());
        let batch = [
            InputEvent {
                event_type: ev::KEY,
                code: 30,
                value: 1,
            },
            InputEvent::SYN_REPORT,
        ];
        assert_eq!(handle.push(&batch).expect("push never fails here"), 0);
        assert_eq!(handle.pending(), 0);
        let stats = handle.stats();
        assert_eq!(stats.dropped_inactive, 2);
        assert_eq!(stats.queued, 0);
        assert_eq!(stats.delivered, 0);

        // An empty push is a no-op, not a drop.
        assert_eq!(handle.push(&[]).expect("no-op"), 0);
        assert_eq!(handle.stats().dropped_inactive, 2);
    }

    #[test]
    fn notify_on_an_inactive_device_is_an_error_not_a_panic() {
        let mut device = InputDevice::absolute_pointer();
        for queue in [EVENT_QUEUE, STATUS_QUEUE, 2, u16::MAX] {
            assert!(matches!(
                device.notify(queue),
                Err(DeviceError::NotActivated)
            ));
        }
    }

    #[test]
    fn reset_of_an_untouched_device_is_harmless() {
        let mut device = InputDevice::keyboard();
        device.write_config(config::SELECT, &[config::VIRTIO_INPUT_CFG_ID_NAME, 4]);
        assert!(device.ack_features(VIRTIO_F_VERSION_1));
        device.reset();
        assert_eq!(device.acked_features(), 0);
        assert_eq!(device.selected(), (config::VIRTIO_INPUT_CFG_UNSET, 0));
        assert_eq!(device.pending(), 0);
        assert!(!device.handle().is_active());
    }

    #[test]
    fn activation_requires_exactly_two_queues() {
        use std::sync::Arc;
        use virtio_core::testing::{guest_memory, TestInterrupt};

        let mem = Arc::new(guest_memory(0x2_0000));
        let mut device = InputDevice::keyboard();
        let resources = DeviceResources {
            mem: Arc::clone(&mem),
            queues: Vec::new(),
            interrupt: Arc::new(TestInterrupt::default()),
            quiesce: virtio_core::Quiesce::new(),
        };
        assert!(matches!(
            device.activate(resources),
            Err(DeviceError::QueueCount {
                expected: 2,
                actual: 0
            })
        ));
    }

    #[test]
    fn handle_debug_does_not_leak_the_lock() {
        let device = InputDevice::keyboard();
        let handle = device.handle();
        let text = format!("{handle:?}");
        assert!(text.contains("Entangled Keyboard"));
        assert!(format!("{device:?}").contains("Keyboard"));
    }

    #[test]
    fn buffer_error_messages_name_the_problem() {
        let error = BufferError::TooSmall { len: 4 };
        assert_eq!(
            error.to_string(),
            "device-writable buffer is 4 bytes, one virtio_input_event needs 8"
        );
        let error = BufferError::WrongDirection { count: 2 };
        assert!(error.to_string().contains("write-only"));
        let error = BufferError::Chain(ChainError::TooLong);
        assert!(error.to_string().starts_with("descriptor chain rejected"));
    }

    #[test]
    fn profiles_advertise_the_codes_display_can_produce() {
        // Every Linux code `display`'s keymap and button map emit must be
        // advertised by exactly one of the two devices.
        let keyboard_codes: Vec<u16> = vec![1, 28, 29, 30, 42, 56, 57, 88, 103, 111, 183, 194, 226];
        for code in keyboard_codes {
            let event = InputEvent {
                event_type: ev::KEY,
                code,
                value: 1,
            };
            assert!(
                Profile::Keyboard.accepts(event),
                "keyboard must accept KEY {code}"
            );
            assert!(!Profile::AbsolutePointer.accepts(event));
        }
        // KEY_SELECT is the keymap's one code above the dense range.
        assert!(Profile::Keyboard.accepts(InputEvent {
            event_type: ev::KEY,
            code: key::SELECT,
            value: 1
        }));
        for code in [btn::LEFT, btn::RIGHT, btn::MIDDLE, btn::SIDE, btn::EXTRA] {
            let event = InputEvent {
                event_type: ev::KEY,
                code,
                value: 1,
            };
            assert!(Profile::AbsolutePointer.accepts(event));
            assert!(!Profile::Keyboard.accepts(event));
        }
    }
}
