//! A userspace Intel 8254 programmable interval timer (backlog WHP-1703).
//!
//! KVM has `KVM_CREATE_PIT2`; WHP has nothing, and Linux does not boot without a
//! PIT on a machine that advertises a dual-8259 (`PCAT_COMPAT` in the MADT — see
//! [`crate::acpi`]). Two places need it, in this order:
//!
//! 1. **TSC calibration.** `quick_pit_calibrate()` puts *channel 2* in mode 0
//!    with latch `0xffff`, opens its gate through port `0x61` bit 0 and watches
//!    the counter's MSB walk down, timing it against RDTSC. If that fails,
//!    `pit_calibrate_tsc()` polls channel 2's output through port `0x61` bit 5.
//!    Both measure *real elapsed time*, so the counter has to be derived from a
//!    host clock and not from a tick count we make up.
//! 2. **`check_timer()`.** `pit_timer_init()` programs *channel 0* as a rate
//!    generator at `HZ` and registers it as `global_clock_event`; then
//!    `setup_IO_APIC()` → `check_timer()` → `timer_irq_works()` enables
//!    interrupts for ten jiffies' worth of milliseconds and requires jiffies to
//!    have advanced by more than four. Without IRQ 0 arriving at roughly the
//!    programmed rate the kernel prints "..MP-BIOS bug: 8254 timer not connected
//!    to IO-APIC", walks its fallbacks and finally panics with "IO-APIC + timer
//!    doesn't work!". The same PIT interrupt then calibrates the local APIC
//!    timer (`calibrate_APIC_clock()` counts on `global_clock_event`).
//!
//! # Model
//!
//! Three counters at the classic ports plus the speaker/gate register:
//!
//! | Port | Access | Register |
//! |---|---|---|
//! | `0x40` | read/write | counter 0 |
//! | `0x41` | read/write | counter 1 (present, unused — DRAM refresh) |
//! | `0x42` | read/write | counter 2 |
//! | `0x43` | write only | mode/command; reads float high |
//! | `0x61` | read/write | NMI status and control: channel 2 gate + speaker enable, channel 2 output |
//!
//! Counters are **computed from elapsed host time**, not stepped: `elapsed_ns *
//! PIT_FREQUENCY_HZ / 1e9` gives the tick count, and the current count follows
//! from the channel's reload value and mode. That is what makes the guest's
//! calibration arrive at the host's real TSC frequency.
//!
//! Only channel 0 has an output pin wired anywhere: IRQ 0, which the machine
//! routes to IOAPIC pin 2 exactly as the MP table and MADT say (PC convention).
//! [`Pit::tick`] delivers the edges that came due and reports when the next one
//! is, and [`PitTimer`] is the host thread that calls it.
//!
//! # Modes
//!
//! Linux drives channel 0 with mode 2 (rate generator, periodic) and mode 0 or 4
//! (one-shot), and channel 2 with mode 0. Mode 3 (square wave) is what a BIOS
//! leaves channel 0 in, so it is accepted too. For interrupt purposes modes 2 and
//! 3 are periodic with a period of `reload` ticks and modes 0, 1, 4 and 5 fire
//! once; the difference between mode 2's and mode 3's output *waveform* is not
//! modelled, because nothing on this machine observes channel 0's pin level.

use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use virtio_core::interrupt::IrqLine;

/// The 8254's input frequency: 1.193182 MHz (14.31818 MHz / 12).
pub const PIT_FREQUENCY_HZ: u64 = 1_193_182;

/// Ports the PIT owns.
pub const PIT_PORT_BASE: u16 = 0x40;
pub const PIT_PORT_LAST: u16 = 0x43;
/// NMI status and control register, whose low bits gate channel 2 and whose
/// bit 5 reports channel 2's output. Not part of the 8254 itself, but nothing
/// else models it and channel 2 is useless without it.
pub const NMI_STATUS_PORT: u16 = 0x61;

const COMMAND_PORT: u16 = 0x43;
const CHANNELS: usize = 3;

/// A reload of 0 means 65536 on an 8254.
const FULL_RELOAD: u64 = 0x1_0000;

/// Upper bound on edges delivered for one [`Pit::tick`] call.
///
/// A host thread that was descheduled for a second while the guest had channel 0
/// running at 1 kHz would otherwise owe a thousand interrupts, and delivering
/// them back to back is both useless (the guest cannot tell) and a way for a
/// stalled host to burn the vCPU on interrupt entry. Linux's clock is monotonic
/// but not obliged to be complete; other VMMs coalesce the same way.
///
/// Sixteen is chosen against the *host's* sleep granularity rather than a round
/// number: measured on Windows 11, `std::thread::sleep` overshoots a 1 ms request
/// by about half a millisecond (it rides a high-resolution waitable timer), and a
/// loaded host can miss a full scheduling quantum of ~15 ms. At the x86_64
/// defconfig's `HZ=1000` that is 15 owed edges, so the cap has to sit above it or
/// the guest's jiffies would drift permanently behind — which is exactly the
/// failure `timer_irq_works()` reports as "8254 timer not connected to IO-APIC".
const MAX_CATCHUP_EDGES: u32 = 16;

/// `0x61` bits the guest may write: channel 2 gate (0) and speaker data (1).
const NMI_WRITE_MASK: u8 = 0x03;
const NMI_GATE2: u8 = 0x01;
/// Channel 2's output, read back at bit 5.
const NMI_OUT2: u8 = 0x20;
/// Refresh-request toggle at bit 4. Some code times loops on it; deriving it
/// from the clock costs nothing and beats a stuck bit.
const NMI_REFRESH: u8 = 0x10;

/// How the guest reads or writes a 16-bit counter through an 8-bit port.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum AccessMode {
    LatchCount,
    LoBoth,
    HiBoth,
    LoThenHi,
}

impl AccessMode {
    fn from_command(rw: u8) -> Self {
        match rw {
            0 => Self::LatchCount,
            1 => Self::LoBoth,
            2 => Self::HiBoth,
            _ => Self::LoThenHi,
        }
    }
}

#[derive(Debug, Clone, Copy)]
struct Channel {
    /// Programmed reload value; 0 means [`FULL_RELOAD`].
    reload: u16,
    mode: u8,
    access: AccessMode,
    /// Host tick count when the counter was (re)armed.
    armed_at_ticks: u64,
    /// True once a full 16-bit reload has been written, i.e. the counter counts.
    armed: bool,
    /// Half-written reload for [`AccessMode::LoThenHi`].
    write_lo: Option<u8>,
    /// Latched counter value, consumed by the next one or two reads.
    latched: Option<u16>,
    /// Which half of a `LoThenHi` read comes next.
    read_hi_next: bool,
    /// Channel 2's gate, driven by port `0x61` bit 0. Channels 0 and 1 are
    /// hard-wired high on a PC.
    gate: bool,
}

impl Channel {
    fn new(gate: bool) -> Self {
        Self {
            reload: 0,
            // Mode 3 (square wave) with reload 0 — the state a PC BIOS leaves
            // channel 0 in, and the one a guest that never programs the PIT
            // expects to find.
            mode: 3,
            access: AccessMode::LoThenHi,
            armed_at_ticks: 0,
            armed: false,
            write_lo: None,
            latched: None,
            read_hi_next: false,
            gate,
        }
    }

    fn reload_ticks(&self) -> u64 {
        if self.reload == 0 {
            FULL_RELOAD
        } else {
            u64::from(self.reload)
        }
    }

    /// Modes 2 and 3 reload themselves; 0, 1, 4 and 5 count once and stop.
    fn is_periodic(&self) -> bool {
        matches!(self.mode & 0x07, 2 | 3 | 6 | 7)
    }

    /// Ticks elapsed since the counter was armed, or `None` while it is not
    /// counting (never loaded, or the gate is low).
    fn elapsed(&self, now_ticks: u64) -> Option<u64> {
        (self.armed && self.gate).then(|| now_ticks.saturating_sub(self.armed_at_ticks))
    }

    /// The value the guest reads out of the counter.
    fn count(&self, now_ticks: u64) -> u16 {
        let reload = self.reload_ticks();
        let Some(elapsed) = self.elapsed(now_ticks) else {
            return self.reload;
        };
        let remaining = if self.is_periodic() {
            reload - (elapsed % reload)
        } else if elapsed >= reload {
            // A one-shot that has expired wraps and keeps counting down from
            // 0xffff, which is what the hardware does and what
            // `pit_expect_msb()` relies on to detect that it overran.
            FULL_RELOAD - ((elapsed - reload) % FULL_RELOAD)
        } else {
            reload - elapsed
        };
        (remaining & 0xffff) as u16
    }

    /// Channel 2's OUT level, as read through port `0x61` bit 5.
    ///
    /// In mode 0 OUT is low while counting and goes high at terminal count —
    /// exactly the transition `pit_calibrate_tsc()` polls for. Periodic modes
    /// toggle; reporting the high half of the period is enough for anything that
    /// looks at all.
    fn output(&self, now_ticks: u64) -> bool {
        let reload = self.reload_ticks();
        match self.elapsed(now_ticks) {
            None => true, // idle counters idle high
            Some(elapsed) if self.is_periodic() => elapsed % reload >= reload / 2,
            Some(elapsed) => elapsed >= reload,
        }
    }

    fn arm(&mut self, now_ticks: u64) {
        self.armed = true;
        self.armed_at_ticks = now_ticks;
    }

    fn write_counter(&mut self, value: u8, now_ticks: u64) {
        match self.access {
            AccessMode::LoBoth => {
                self.reload = u16::from(value);
                self.arm(now_ticks);
            }
            AccessMode::HiBoth => {
                self.reload = u16::from(value) << 8;
                self.arm(now_ticks);
            }
            // A latch-count command is not a write mode; treat a write as
            // lo-then-hi rather than dropping it.
            AccessMode::LoThenHi | AccessMode::LatchCount => match self.write_lo.take() {
                None => self.write_lo = Some(value),
                Some(lo) => {
                    self.reload = u16::from_le_bytes([lo, value]);
                    self.arm(now_ticks);
                }
            },
        }
    }

    fn read_counter(&mut self, now_ticks: u64) -> u8 {
        let value = self.latched.unwrap_or_else(|| self.count(now_ticks));
        match self.access {
            AccessMode::LoBoth => {
                self.latched = None;
                (value & 0xff) as u8
            }
            AccessMode::HiBoth => {
                self.latched = None;
                (value >> 8) as u8
            }
            AccessMode::LoThenHi | AccessMode::LatchCount => {
                if self.read_hi_next {
                    self.read_hi_next = false;
                    self.latched = None;
                    (value >> 8) as u8
                } else {
                    self.read_hi_next = true;
                    (value & 0xff) as u8
                }
            }
        }
    }

    /// Status byte for the 8254 read-back command: OUT, null count, RW, mode,
    /// BCD.
    fn status(&self, now_ticks: u64) -> u8 {
        let rw = match self.access {
            AccessMode::LatchCount => 0,
            AccessMode::LoBoth => 1,
            AccessMode::HiBoth => 2,
            AccessMode::LoThenHi => 3,
        };
        (u8::from(self.output(now_ticks)) << 7)
            | (u8::from(!self.armed) << 6)
            | (rw << 4)
            | ((self.mode & 0x07) << 1)
    }
}

struct PitState {
    channels: [Channel; CHANNELS],
    /// Guest-written bits of port `0x61`.
    nmi_control: u8,
    /// Host tick count at which channel 0's next output edge is due, while it
    /// has one.
    next_edge_ticks: Option<u64>,
}

/// The machine's 8254.
///
/// Shared (`Arc`) between the bus, which serves the guest's port I/O, and
/// [`PitTimer`]'s thread, which delivers channel 0's edges.
pub struct Pit {
    state: Mutex<PitState>,
    irq0: Arc<dyn IrqLine>,
    clock: Clock,
    /// Channel 0 output edges handed to the interrupt line. Diagnostics: a boot
    /// that hangs in `check_timer()` looks completely different depending on
    /// whether this is zero.
    edges: AtomicU64,
    /// Set while the VM is paused (ADR-0005): the timer thread keeps turning
    /// but delivers nothing, and [`Pit::set_paused`] re-arms the schedule on the
    /// way out.
    paused: AtomicBool,
}

/// The PIT's time source. Real by default; a test drives it by hand so counter
/// and interrupt behaviour is asserted deterministically rather than by sleeping.
enum Clock {
    Host(Instant),
    #[cfg(test)]
    Fake(AtomicU64),
}

impl Clock {
    fn ticks(&self) -> u64 {
        match self {
            // Nanoseconds since the PIT was created, converted at the 8254's
            // input frequency. `u128` because ns * 1.19e6 overflows `u64` after
            // ~4 hours.
            Self::Host(epoch) => {
                let ns = epoch.elapsed().as_nanos();
                u64::try_from(ns * u128::from(PIT_FREQUENCY_HZ) / 1_000_000_000).unwrap_or(u64::MAX)
            }
            #[cfg(test)]
            Self::Fake(ticks) => ticks.load(Ordering::Acquire),
        }
    }
}

impl Pit {
    /// Creates the PIT with its channel-0 output wired to `irq0` — the machine's
    /// IOAPIC pin 2.
    pub fn new(irq0: Arc<dyn IrqLine>) -> Arc<Self> {
        Self::with_clock(irq0, Clock::Host(Instant::now()))
    }

    fn with_clock(irq0: Arc<dyn IrqLine>, clock: Clock) -> Arc<Self> {
        Arc::new(Self {
            state: Mutex::new(PitState {
                channels: [Channel::new(true), Channel::new(true), Channel::new(false)],
                nmi_control: 0,
                next_edge_ticks: None,
            }),
            irq0,
            clock,
            edges: AtomicU64::new(0),
            paused: AtomicBool::new(false),
        })
    }

    /// True when `port` belongs to the PIT (including the channel-2 gate
    /// register).
    pub fn contains(port: u16) -> bool {
        (PIT_PORT_BASE..=PIT_PORT_LAST).contains(&port) || port == NMI_STATUS_PORT
    }

    /// Channel 0 output edges delivered so far.
    pub fn edges(&self) -> u64 {
        self.edges.load(Ordering::Acquire)
    }

    /// Stops or restarts channel-0 delivery while the VM is paused
    /// (ADR-0005).
    ///
    /// The PIT's counter is derived from *host* time, which keeps running while
    /// a VM is held, so an un-gated timer thread would deliver every tick the
    /// pause was worth the instant the guest came back. (`tick`'s
    /// `MAX_CATCHUP_EDGES` already bounds that backlog, but "bounded" is not
    /// "none".) Resuming therefore also re-arms the next edge from the current
    /// time rather than from where the guest left off, which is the same thing
    /// the catch-up limiter does and the closest a free-running counter gets to
    /// having been stopped.
    pub fn set_paused(&self, paused: bool) {
        self.paused.store(paused, Ordering::Release);
        if paused {
            return;
        }
        let now = self.clock.ticks();
        let Ok(mut state) = self.state.lock() else {
            tracing::error!("PIT lock is poisoned; channel 0 keeps its old schedule");
            return;
        };
        if state.next_edge_ticks.is_some() {
            state.next_edge_ticks = next_edge(&state.channels[0], now);
        }
    }

    /// True while [`Self::set_paused`] has channel-0 delivery stopped.
    pub fn is_paused(&self) -> bool {
        self.paused.load(Ordering::Acquire)
    }

    /// Machine reset (ADR-0005): the three channels back to un-programmed, the
    /// NMI/speaker control byte cleared and no edge scheduled.
    ///
    /// `edges` survives: it counts what this *host* has delivered for the whole
    /// run, and a boot test that resets in the middle still wants to know the
    /// 8254 fired at all.
    pub fn reset(&self) {
        self.paused.store(false, Ordering::Release);
        let Ok(mut state) = self.state.lock() else {
            tracing::error!("PIT lock is poisoned; the channels stay as they were");
            return;
        };
        state.channels = [Channel::new(true), Channel::new(true), Channel::new(false)];
        state.nmi_control = 0;
        state.next_edge_ticks = None;
    }

    /// The three channels and the NMI/speaker byte, for a snapshot (ADR-0006).
    ///
    /// Every time in here is **relative to now**. A channel is armed at an
    /// absolute host tick count taken from an `Instant` this process owns, and
    /// the process that restores the snapshot has a different one — so what is
    /// written down is how long ago each channel was armed and how long until
    /// the next edge, and the restore anchors both to its own epoch. A guest
    /// half-way through a 10 ms tick comes back half-way through it.
    pub fn save_state(&self) -> crate::state::SavedPit {
        let now = self.clock.ticks();
        let Ok(state) = self.state.lock() else {
            tracing::error!("PIT lock is poisoned; saving an un-programmed 8254");
            return crate::state::SavedPit::default();
        };
        crate::state::SavedPit {
            channels: state
                .channels
                .iter()
                .map(|channel| crate::state::SavedPitChannel {
                    reload: channel.reload,
                    mode: channel.mode,
                    access: match channel.access {
                        AccessMode::LatchCount => 0,
                        AccessMode::LoBoth => 1,
                        AccessMode::HiBoth => 2,
                        AccessMode::LoThenHi => 3,
                    },
                    ticks_since_armed: now.saturating_sub(channel.armed_at_ticks),
                    armed: channel.armed,
                    write_lo: channel.write_lo,
                    latched: channel.latched,
                    read_hi_next: channel.read_hi_next,
                    gate: channel.gate,
                })
                .collect(),
            nmi_control: state.nmi_control,
            ticks_to_next_edge: state.next_edge_ticks.map(|due| due.saturating_sub(now)),
        }
    }

    /// Puts it back, anchored to this process's clock.
    pub fn load_state(
        &self,
        saved: &crate::state::SavedPit,
    ) -> Result<(), crate::state::StateError> {
        let now = self.clock.ticks();
        let Ok(mut state) = self.state.lock() else {
            return Err(crate::state::StateError::Poisoned("the 8254"));
        };
        if saved.channels.len() != state.channels.len() {
            return Err(crate::state::StateError::Count {
                what: "8254 channels",
                snapshot: saved.channels.len(),
                current: state.channels.len(),
            });
        }
        for (channel, saved) in state.channels.iter_mut().zip(&saved.channels) {
            channel.access = match saved.access {
                0 => AccessMode::LatchCount,
                1 => AccessMode::LoBoth,
                2 => AccessMode::HiBoth,
                3 => AccessMode::LoThenHi,
                other => {
                    return Err(crate::state::StateError::BadValue {
                        what: "8254 access mode",
                        value: u64::from(other),
                    })
                }
            };
            channel.reload = saved.reload;
            channel.mode = saved.mode;
            channel.armed_at_ticks = now.saturating_sub(saved.ticks_since_armed);
            channel.armed = saved.armed;
            channel.write_lo = saved.write_lo;
            channel.latched = saved.latched;
            channel.read_hi_next = saved.read_hi_next;
            channel.gate = saved.gate;
        }
        state.nmi_control = saved.nmi_control;
        state.next_edge_ticks = saved
            .ticks_to_next_edge
            .map(|remaining| now.saturating_add(remaining));
        Ok(())
    }

    /// Guest write. Never fails towards the guest.
    pub fn io_write(&self, port: u16, value: u8) {
        let now = self.clock.ticks();
        let Ok(mut state) = self.state.lock() else {
            tracing::error!("PIT lock is poisoned; dropping guest write");
            return;
        };
        match port {
            COMMAND_PORT => command(&mut state, value, now),
            NMI_STATUS_PORT => {
                state.nmi_control = value & NMI_WRITE_MASK;
                let gate = value & NMI_GATE2 != 0;
                let channel = &mut state.channels[2];
                if gate && !channel.gate {
                    // A rising gate restarts the count, which is exactly what
                    // `quick_pit_calibrate()` uses to start its measurement.
                    channel.armed_at_ticks = now;
                }
                channel.gate = gate;
            }
            _ => {
                let index = usize::from(port - PIT_PORT_BASE);
                if let Some(channel) = state.channels.get_mut(index) {
                    channel.write_counter(value, now);
                    if index == 0 {
                        state.next_edge_ticks = next_edge(&state.channels[0], now);
                    }
                }
            }
        }
    }

    /// Guest read.
    pub fn io_read(&self, port: u16) -> u8 {
        let now = self.clock.ticks();
        let Ok(mut state) = self.state.lock() else {
            tracing::error!("PIT lock is poisoned; reading 0xff");
            return 0xff;
        };
        match port {
            // The command register is write-only; an unclaimed ISA read floats.
            COMMAND_PORT => 0xff,
            NMI_STATUS_PORT => {
                let out2 = u8::from(state.channels[2].output(now)) * NMI_OUT2;
                let refresh = if now % 32 < 16 { NMI_REFRESH } else { 0 };
                state.nmi_control | out2 | refresh
            }
            _ => {
                let index = usize::from(port - PIT_PORT_BASE);
                state
                    .channels
                    .get_mut(index)
                    .map_or(0xff, |channel| channel.read_counter(now))
            }
        }
    }

    /// Delivers every channel-0 output edge that has come due and returns how
    /// long until the next one, or `None` when channel 0 has no pending edge
    /// (never programmed, gated off, or a one-shot that already fired).
    ///
    /// Split out from the timer thread so the edge arithmetic is testable without
    /// waiting on a real clock.
    pub fn tick(&self) -> Option<Duration> {
        if self.is_paused() {
            // A paused VM makes no progress, and that has to include its
            // timekeeping: an interrupt delivered now would sit in a parked
            // vCPU's local APIC and fire the instant it resumed.
            return None;
        }
        let now = self.clock.ticks();
        let edges = {
            let Ok(mut state) = self.state.lock() else {
                tracing::error!("PIT lock is poisoned; channel 0 stops ticking");
                return None;
            };
            let mut edges = 0;
            while let Some(due) = state.next_edge_ticks {
                if due > now {
                    break;
                }
                edges += 1;
                let channel = &state.channels[0];
                state.next_edge_ticks = if channel.is_periodic() {
                    Some(due + channel.reload_ticks())
                } else {
                    None
                };
                if edges >= MAX_CATCHUP_EDGES {
                    // Skip whatever else is owed: resynchronise on the current
                    // time instead of delivering a backlog.
                    if let Some(due) = state.next_edge_ticks {
                        if due <= now {
                            state.next_edge_ticks = next_edge(&state.channels[0], now);
                        }
                    }
                    break;
                }
            }
            edges
        };
        for _ in 0..edges {
            if let Err(e) = self.irq0.trigger() {
                tracing::warn!(error = %e, "raising PIT IRQ 0 failed");
                break;
            }
            self.edges.fetch_add(1, Ordering::AcqRel);
        }
        let state = self.state.lock().ok()?;
        let due = state.next_edge_ticks?;
        Some(ticks_to_duration(due.saturating_sub(self.clock.ticks())))
    }
}

/// Writes the mode/command register: either a read-back command, a latch, or a
/// full channel reprogram.
fn command(state: &mut PitState, value: u8, now: u64) {
    let select = (value >> 6) & 0x03;
    if select == 3 {
        // 8254 read-back command: latch counts and/or status for the selected
        // channels. `pit_verify_msb()` does not use it, but `i8253`'s
        // `pit_read_status()` does.
        let latch_count = value & 0x20 == 0;
        let latch_status = value & 0x10 == 0;
        for (index, channel) in state.channels.iter_mut().enumerate() {
            if value & (1 << (index + 1)) == 0 {
                continue;
            }
            if latch_status {
                channel.latched = Some(u16::from(channel.status(now)));
            } else if latch_count && channel.latched.is_none() {
                channel.latched = Some(channel.count(now));
            }
            channel.read_hi_next = false;
        }
        return;
    }

    let index = usize::from(select);
    let Some(channel) = state.channels.get_mut(index) else {
        return;
    };
    let access = AccessMode::from_command((value >> 4) & 0x03);
    if access == AccessMode::LatchCount {
        if channel.latched.is_none() {
            channel.latched = Some(channel.count(now));
        }
        channel.read_hi_next = false;
        return;
    }
    channel.access = access;
    channel.mode = (value >> 1) & 0x07;
    // Reprogramming stops the counter until a new reload is written; the old
    // count is meaningless and a stale edge must not fire against the new mode.
    channel.armed = false;
    channel.write_lo = None;
    channel.latched = None;
    channel.read_hi_next = false;
    if index == 0 {
        state.next_edge_ticks = None;
    }
}

/// When channel 0's next output edge is due, given it was armed at
/// `channel.armed_at_ticks`.
fn next_edge(channel: &Channel, now: u64) -> Option<u64> {
    if !channel.armed || !channel.gate {
        return None;
    }
    let reload = channel.reload_ticks();
    let elapsed = now.saturating_sub(channel.armed_at_ticks);
    if channel.is_periodic() {
        let periods = elapsed / reload + 1;
        Some(channel.armed_at_ticks + periods * reload)
    } else if elapsed < reload {
        Some(channel.armed_at_ticks + reload)
    } else {
        None
    }
}

fn ticks_to_duration(ticks: u64) -> Duration {
    Duration::from_nanos(
        u64::try_from(u128::from(ticks) * 1_000_000_000 / u128::from(PIT_FREQUENCY_HZ))
            .unwrap_or(u64::MAX),
    )
}

/// The host thread that drives channel 0.
///
/// Sleeps until the next edge is due, or [`IDLE_POLL`] when channel 0 has none —
/// polling rather than waiting on a condition variable because the guest arms the
/// counter from a vCPU thread inside a port-I/O exit, and `IDLE_POLL` of extra
/// latency on the very first tick is invisible next to the tens of milliseconds
/// `check_timer()` allows. Joining on `Drop` is what guarantees the thread cannot
/// outlive the interrupt line it triggers.
pub struct PitTimer {
    stop: Arc<std::sync::atomic::AtomicBool>,
    handle: Option<std::thread::JoinHandle<()>>,
}

/// How long the timer thread sleeps while channel 0 is idle.
const IDLE_POLL: Duration = Duration::from_millis(10);

/// Floor on a sleep, so a guest that programs a 1-tick reload cannot spin the
/// host thread at 1.19 MHz.
const MIN_SLEEP: Duration = Duration::from_micros(200);

impl PitTimer {
    /// Spawns the thread driving `pit`.
    pub fn start(pit: Arc<Pit>) -> std::io::Result<Self> {
        let stop = Arc::new(std::sync::atomic::AtomicBool::new(false));
        let flag = Arc::clone(&stop);
        let handle = std::thread::Builder::new()
            .name("pit".into())
            .spawn(move || {
                while !flag.load(Ordering::Acquire) {
                    let sleep = pit.tick().unwrap_or(IDLE_POLL).max(MIN_SLEEP);
                    std::thread::sleep(sleep.min(IDLE_POLL));
                }
            })?;
        Ok(Self {
            stop,
            handle: Some(handle),
        })
    }
}

impl Drop for PitTimer {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::Release);
        if let Some(handle) = self.handle.take() {
            let _ = handle.join();
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use virtio_core::interrupt::InterruptError;

    #[derive(Default)]
    struct Counter(AtomicU64);

    impl IrqLine for Counter {
        fn trigger(&self) -> Result<(), InterruptError> {
            self.0.fetch_add(1, Ordering::AcqRel);
            Ok(())
        }
    }

    struct Harness {
        pit: Arc<Pit>,
        irq: Arc<Counter>,
    }

    impl Harness {
        fn new() -> Self {
            let irq = Arc::new(Counter::default());
            let pit = Pit::with_clock(irq.clone(), Clock::Fake(AtomicU64::new(0)));
            Self { pit, irq }
        }

        fn advance(&self, ticks: u64) {
            match &self.pit.clock {
                Clock::Fake(now) => {
                    now.fetch_add(ticks, Ordering::AcqRel);
                }
                Clock::Host(_) => unreachable!("test harness uses the fake clock"),
            }
        }

        fn irqs(&self) -> u64 {
            self.irq.0.load(Ordering::Acquire)
        }

        /// What `clockevent_i8253_init` writes for a periodic tick: channel 0,
        /// lo-then-hi, mode 2.
        fn program_channel0_periodic(&self, latch: u16) {
            self.pit.io_write(COMMAND_PORT, 0x34);
            self.pit.io_write(PIT_PORT_BASE, (latch & 0xff) as u8);
            self.pit.io_write(PIT_PORT_BASE, (latch >> 8) as u8);
        }
    }

    /// The reload value Linux uses for HZ=250 (`PIT_TICK_RATE / 250`).
    const LATCH_250HZ: u16 = 4772;

    #[test]
    fn periodic_channel0_fires_at_the_programmed_rate() {
        let h = Harness::new();
        h.program_channel0_periodic(LATCH_250HZ);
        assert_eq!(h.irqs(), 0, "no edge before the first period elapses");

        // Just short of one period: still nothing.
        h.advance(u64::from(LATCH_250HZ) - 1);
        h.pit.tick();
        assert_eq!(h.irqs(), 0);

        h.advance(1);
        h.pit.tick();
        assert_eq!(h.irqs(), 1);

        // Three more periods, delivered one per tick call.
        for expected in 2..=4 {
            h.advance(u64::from(LATCH_250HZ));
            h.pit.tick();
            assert_eq!(h.irqs(), expected);
        }
    }

    /// `timer_irq_works()` needs more than four jiffies inside ten jiffies'
    /// worth of milliseconds. Asserted in PIT ticks so the test states the
    /// kernel's actual acceptance condition.
    #[test]
    fn ten_jiffies_of_time_yields_more_than_four_interrupts() {
        let h = Harness::new();
        h.program_channel0_periodic(LATCH_250HZ);
        // 10 jiffies at HZ=250 = 40 ms.
        let window = PIT_FREQUENCY_HZ * 40 / 1000;
        let mut delivered = 0;
        // The thread wakes per edge; simulate that rather than one giant jump,
        // because coalescing deliberately caps a single catch-up.
        for _ in 0..12 {
            h.advance(window / 12);
            h.pit.tick();
            delivered = h.irqs();
        }
        assert!(
            delivered > 4,
            "check_timer() would fail: {delivered} interrupts in 40 ms"
        );
    }

    /// A host stall must not turn into an interrupt storm.
    #[test]
    fn a_long_stall_coalesces_instead_of_flooding() {
        let h = Harness::new();
        h.program_channel0_periodic(LATCH_250HZ);
        h.advance(PIT_FREQUENCY_HZ * 5); // five seconds of missed edges
        h.pit.tick();
        assert_eq!(h.irqs(), u64::from(MAX_CATCHUP_EDGES));
        // And the timer resynchronises rather than staying behind forever.
        h.advance(u64::from(LATCH_250HZ));
        h.pit.tick();
        assert_eq!(h.irqs(), u64::from(MAX_CATCHUP_EDGES) + 1);
    }

    #[test]
    fn one_shot_channel0_fires_once() {
        let h = Harness::new();
        // Mode 0, lo-then-hi, channel 0 — `i8253`'s one-shot programming.
        h.pit.io_write(COMMAND_PORT, 0x30);
        h.pit.io_write(PIT_PORT_BASE, 0x00);
        h.pit.io_write(PIT_PORT_BASE, 0x10); // 0x1000 ticks
        h.advance(0x1000);
        h.pit.tick();
        assert_eq!(h.irqs(), 1);
        h.advance(0x1_0000);
        assert_eq!(h.pit.tick(), None, "a fired one-shot has no next edge");
        assert_eq!(h.irqs(), 1);
    }

    /// Reprogramming the mode must disarm the counter: an edge scheduled under
    /// the old reload firing against the new mode is how a guest ends up with a
    /// timer that ticks at the wrong rate.
    #[test]
    fn reprogramming_the_mode_disarms_channel0() {
        let h = Harness::new();
        h.program_channel0_periodic(LATCH_250HZ);
        h.pit.io_write(COMMAND_PORT, 0x30); // mode 0, no reload yet
        h.advance(PIT_FREQUENCY_HZ);
        assert_eq!(h.pit.tick(), None);
        assert_eq!(h.irqs(), 0);
    }

    /// `quick_pit_calibrate()`: gate channel 2 through 0x61, mode 0 with latch
    /// 0xffff, then watch the MSB walk down with unlatched lo/hi reads.
    #[test]
    fn channel2_counts_down_for_quick_pit_calibrate() {
        let h = Harness::new();
        h.pit.io_write(NMI_STATUS_PORT, 0x01); // gate high, speaker off
        h.pit.io_write(COMMAND_PORT, 0xb0); // channel 2, lo/hi, mode 0
        h.pit.io_write(PIT_PORT_BASE + 2, 0xff);
        h.pit.io_write(PIT_PORT_BASE + 2, 0xff);

        // pit_verify_msb(): discard the LSB, then read the MSB.
        let msb = |h: &Harness| {
            h.pit.io_read(PIT_PORT_BASE + 2);
            h.pit.io_read(PIT_PORT_BASE + 2)
        };
        assert_eq!(msb(&h), 0xff);
        h.advance(0x0100); // 256 ticks: MSB drops by one
        assert_eq!(msb(&h), 0xfe);
        h.advance(0x0f00);
        assert_eq!(msb(&h), 0xef);
    }

    /// `pit_calibrate_tsc()` polls OUT2 at 0x61 bit 5 and needs it to be low
    /// while channel 2 counts and high at terminal count.
    #[test]
    fn channel2_output_reports_terminal_count_on_port_61() {
        let h = Harness::new();
        h.pit.io_write(NMI_STATUS_PORT, 0x01);
        h.pit.io_write(COMMAND_PORT, 0xb0);
        h.pit.io_write(PIT_PORT_BASE + 2, 0x00);
        h.pit.io_write(PIT_PORT_BASE + 2, 0x10); // 0x1000 ticks

        assert_eq!(
            h.pit.io_read(NMI_STATUS_PORT) & NMI_OUT2,
            0,
            "OUT2 low while counting"
        );
        // The gate bit the guest wrote must read back, or Linux's
        // read-modify-write of 0x61 loses the speaker state.
        assert_eq!(h.pit.io_read(NMI_STATUS_PORT) & NMI_WRITE_MASK, 0x01);

        h.advance(0x1000);
        assert_ne!(
            h.pit.io_read(NMI_STATUS_PORT) & NMI_OUT2,
            0,
            "OUT2 must go high at terminal count"
        );
    }

    /// Channel 2 does not count while its gate is low, and a rising gate
    /// restarts it — that is what makes the calibration window start where the
    /// guest thinks it does.
    #[test]
    fn channel2_gate_starts_and_stops_the_count() {
        let h = Harness::new();
        h.pit.io_write(COMMAND_PORT, 0xb0);
        h.pit.io_write(PIT_PORT_BASE + 2, 0xff);
        h.pit.io_write(PIT_PORT_BASE + 2, 0xff);
        h.advance(0x1000);
        // Gate still low: the counter shows the reload value, untouched.
        h.pit.io_read(PIT_PORT_BASE + 2);
        assert_eq!(h.pit.io_read(PIT_PORT_BASE + 2), 0xff);

        h.pit.io_write(NMI_STATUS_PORT, 0x01);
        h.advance(0x0200);
        h.pit.io_read(PIT_PORT_BASE + 2);
        assert_eq!(h.pit.io_read(PIT_PORT_BASE + 2), 0xfd);
    }

    /// The latch command freezes the count for the next read pair, so a guest
    /// reading lo then hi never sees a torn value.
    #[test]
    fn latch_command_freezes_the_counter() {
        let h = Harness::new();
        h.program_channel0_periodic(0);
        h.advance(0x1234);
        h.pit.io_write(COMMAND_PORT, 0x00); // latch channel 0
        let lo = h.pit.io_read(PIT_PORT_BASE);
        h.advance(0x5000); // time moves on between the two reads
        let hi = h.pit.io_read(PIT_PORT_BASE);
        assert_eq!(
            u16::from_le_bytes([lo, hi]),
            (FULL_RELOAD - 0x1234) as u16,
            "the latched value must not follow the clock"
        );
    }

    #[test]
    fn readback_status_reports_mode_and_output() {
        let h = Harness::new();
        h.program_channel0_periodic(LATCH_250HZ);
        // Read-back: status only (bit 4 clear, bit 5 set), channel 0 (bit 1).
        h.pit.io_write(COMMAND_PORT, 0xc0 | 0x20 | 0x02);
        let status = h.pit.io_read(PIT_PORT_BASE);
        assert_eq!((status >> 1) & 0x07, 2, "mode 2");
        assert_eq!((status >> 4) & 0x03, 3, "lo-then-hi access mode");
        assert_eq!(status & 0x40, 0, "null count clear once armed");
    }

    #[test]
    fn port_ownership_covers_the_gate_register_only() {
        for port in [0x40u16, 0x41, 0x42, 0x43, 0x61] {
            assert!(Pit::contains(port), "{port:#x} must be claimed");
        }
        for port in [0x3fu16, 0x44, 0x60, 0x62, 0x70] {
            assert!(!Pit::contains(port), "{port:#x} must not be claimed");
        }
    }

    /// The command register is write-only; a read must float high rather than
    /// return a channel value.
    #[test]
    fn command_register_reads_float_high() {
        let h = Harness::new();
        assert_eq!(h.pit.io_read(COMMAND_PORT), 0xff);
    }

    /// The real clock must produce a monotonically advancing tick count at
    /// roughly the 8254's frequency — the property every calibration depends on,
    /// and the one the fake clock cannot prove.
    #[test]
    fn the_host_clock_ticks_at_the_pit_frequency() {
        let clock = Clock::Host(Instant::now());
        let first = clock.ticks();
        std::thread::sleep(Duration::from_millis(30));
        let elapsed = clock.ticks() - first;
        let expected = PIT_FREQUENCY_HZ * 30 / 1000;
        assert!(
            elapsed >= expected / 2 && elapsed < expected * 8,
            "30 ms produced {elapsed} ticks, expected around {expected}"
        );
    }

    /// The timer thread must actually deliver edges and must stop on drop.
    #[test]
    fn the_timer_thread_delivers_and_joins() {
        let irq = Arc::new(Counter::default());
        let pit = Pit::new(irq.clone());
        // 1 kHz, so a 100 ms wait is many periods even on a loaded host.
        pit.io_write(COMMAND_PORT, 0x34);
        let latch = (PIT_FREQUENCY_HZ / 1000) as u16;
        pit.io_write(PIT_PORT_BASE, (latch & 0xff) as u8);
        pit.io_write(PIT_PORT_BASE, (latch >> 8) as u8);

        let timer = PitTimer::start(Arc::clone(&pit)).unwrap();
        std::thread::sleep(Duration::from_millis(100));
        drop(timer);
        let delivered = irq.0.load(Ordering::Acquire);
        assert!(delivered > 4, "only {delivered} edges in 100 ms at 1 kHz");
        assert_eq!(delivered, pit.edges());
    }
}
