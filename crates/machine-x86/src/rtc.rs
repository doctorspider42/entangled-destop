//! MC146818-compatible RTC/CMOS at the classic index/data ports 0x70/0x71
//! (backlog UEFI-1802).
//!
//! A UEFI firmware must implement `EFI_RUNTIME_SERVICES.GetTime()`, and on a PC
//! platform that is `PcAtChipsetPkg/PcatRealTimeClockRuntimeDxe` talking to this
//! device. Without it `PcRtcInit()` spins in `RtcWaitToUpdate()` until it times
//! out and returns `EFI_DEVICE_ERROR`, which is a hard failure in the driver's
//! entry point.
//!
//! Two details of EDK2's accessor are load-bearing and easy to get wrong:
//!
//! * `IoRtcRead()` *reads* the index port to preserve its NMI-disable bit
//!   before OR-ing in the register number, so port 0x70 must read back the
//!   value last written. A floating `0xff` there turns every subsequent
//!   register number into `reg | 0x80` and the driver reads nonsense.
//! * Register A's UIP bit must be clear (we are never mid-update — the time is
//!   computed on demand) and register D's VRT bit must be set, or the driver
//!   concludes the battery is dead.
//!
//! The clock is read-only towards the guest: guest writes to the time
//! registers are accepted and dropped rather than being allowed to reach the
//! host clock. The guest can freely use the CMOS scratch RAM above the
//! register block, which is what firmware expects of it.
//!
//! # Interrupts (ADR-0008 §3)
//!
//! Linux and EDK2 never asked this RTC to interrupt. Windows does: on a
//! machine without an HPET its HAL takes the RTC's **periodic interrupt** as
//! the system clock, programs register A's rate, sets `PIE` in register B and
//! waits for IRQ 8. Without it the HAL fails its timer bring-up and the kernel
//! stops with `HAL_INITIALIZATION_FAILED` (0x5C, parameter 0x110) before the
//! display is up — a boot that sits on the firmware logo forever.
//!
//! So register B keeps `PIE` and `UIE`, register C reports `PF`, `UF` and
//! `IRQF` and clears them when read, and an [`RtcTimer`] thread drives the
//! periodic and update-ended events onto the line [`RtcInterrupts::connect`]
//! was given. The line is an edge: one per `IRQF` going from clear to set,
//! which is what a guest that reads register C in its handler expects. `AIE`
//! is accepted and never fires; nothing has asked for the alarm.

use std::sync::atomic::{AtomicBool, AtomicU32, AtomicU64, AtomicU8, Ordering};
use std::sync::{Arc, OnceLock};
use std::thread::JoinHandle;
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use virtio_core::interrupt::IrqLine;

/// CMOS/RTC index port. Bit 7 is the NMI-disable line, not part of the address.
pub const RTC_INDEX_PORT: u16 = 0x70;
/// CMOS/RTC data port.
pub const RTC_DATA_PORT: u16 = 0x71;

/// Bit 7 of the index port: NMI disable, not part of the register number.
const NMI_DISABLE: u8 = 0x80;
/// Mask selecting the register number out of the index port.
const INDEX_MASK: u8 = !NMI_DISABLE;

// Register numbers (MC146818).
const REG_SECONDS: u8 = 0x00;
const REG_MINUTES: u8 = 0x02;
const REG_HOURS: u8 = 0x04;
const REG_DAY_OF_WEEK: u8 = 0x06;
const REG_DAY_OF_MONTH: u8 = 0x07;
const REG_MONTH: u8 = 0x08;
const REG_YEAR: u8 = 0x09;
const REG_A: u8 = 0x0a;
const REG_B: u8 = 0x0b;
const REG_C: u8 = 0x0c;
const REG_D: u8 = 0x0d;
/// De-facto standard century register (as used by ACPI's FADT century field —
/// `crate::acpi` publishes this index there, so the two cannot drift).
pub const REG_CENTURY: u8 = 0x32;

/// Register B bits.
const REG_B_DSE: u8 = 1 << 0; // daylight saving enable
const REG_B_24H: u8 = 1 << 1; // 1 = 24-hour mode
const REG_B_BINARY: u8 = 1 << 2; // 1 = binary, 0 = BCD
const REG_B_UIE: u8 = 1 << 4; // update-ended interrupt enable
const REG_B_AIE: u8 = 1 << 5; // alarm interrupt enable (never fires)
const REG_B_PIE: u8 = 1 << 6; // periodic interrupt enable

/// Register C flags: update-ended, periodic, and "an enabled one is pending".
const REG_C_UF: u8 = 1 << 4;
const REG_C_PF: u8 = 1 << 6;
const REG_C_IRQF: u8 = 1 << 7;

/// Register D bit 7: valid RAM and time. Must be set or firmware treats the
/// clock as dead.
const REG_D_VRT: u8 = 1 << 7;

/// Power-on register B: 24-hour mode, BCD encoding — the combination every PC
/// firmware assumes when it has no NVRAM to remember otherwise.
const DEFAULT_REG_B: u8 = REG_B_24H;

/// Size of the CMOS register file.
const CMOS_SIZE: usize = 0x80;

/// A broken-down UTC time, the way the RTC reports it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct RtcTime {
    pub year: u16,
    pub month: u8,
    pub day: u8,
    pub hour: u8,
    pub minute: u8,
    pub second: u8,
    /// 1 = Sunday, per the MC146818 convention.
    pub weekday: u8,
}

/// Converts Unix time to a UTC calendar date.
///
/// Howard Hinnant's `civil_from_days`, which is exact for the whole range of
/// `i64` days and needs no lookup tables or leap-year special cases.
pub fn civil_from_unix(unix_seconds: i64) -> RtcTime {
    let days = unix_seconds.div_euclid(86_400);
    let secs_of_day = unix_seconds.rem_euclid(86_400);

    // Shift the epoch to 0000-03-01 so leap days land at the end of the cycle.
    let z = days + 719_468;
    let era = z.div_euclid(146_097);
    let doe = z.rem_euclid(146_097); // day of era, 0..=146096
    let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365; // 0..=399
    let y = yoe + era * 400;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100); // 0..=365, March-based
    let mp = (5 * doy + 2) / 153; // 0..=11, 0 = March
    let d = doy - (153 * mp + 2) / 5 + 1;
    let m = if mp < 10 { mp + 3 } else { mp - 9 };
    let year = if m <= 2 { y + 1 } else { y };

    // 1970-01-01 was a Thursday; the MC146818 numbers Sunday as 1.
    let weekday = ((days + 4).rem_euclid(7) + 1) as u8;

    RtcTime {
        year: year.clamp(0, u16::MAX as i64) as u16,
        month: m as u8,
        day: d as u8,
        hour: (secs_of_day / 3600) as u8,
        minute: ((secs_of_day % 3600) / 60) as u8,
        second: (secs_of_day % 60) as u8,
        weekday,
    }
}

fn to_bcd(value: u8) -> u8 {
    ((value / 10) << 4) | (value % 10)
}

/// The periodic rate register A selects, in Hz, or `None` when the divider
/// chain is not running or the rate is "none".
fn periodic_hz(reg_a: u8) -> Option<u64> {
    // DV = 010 is the 32.768 kHz time base; any other divider stops the chain.
    if (reg_a >> 4) & 0x7 != 0b010 {
        return None;
    }
    match reg_a & 0xf {
        0 => None,
        // The MC146818 quirk: rates 1 and 2 repeat rates 8 and 9.
        1 => Some(256),
        2 => Some(128),
        rs => Some(32_768 >> (rs - 1)),
    }
}

/// The RTC's interrupt half, shared between the register file (on a vCPU
/// thread) and the [`RtcTimer`] that makes the events happen.
#[derive(Default)]
pub struct RtcInterrupts {
    /// Register A as the guest wrote it: the rate select and the divider.
    reg_a: AtomicU8,
    /// Register B: only its interrupt-enable bits matter here.
    reg_b: AtomicU8,
    /// Register C: what happened since the guest last read it.
    flags: AtomicU8,
    line: OnceLock<Arc<dyn IrqLine>>,
    /// How many times the line was raised, for tests and `doctor`.
    raised: AtomicU64,
    /// Periodic events that came due and have not reached the guest yet: the
    /// host thread woke late, or the guest had not acknowledged the one
    /// before. Bounded by [`MAX_OWED`].
    owed: AtomicU32,
}

/// The most periodic events kept owed: 16 ms of a 1024 Hz clock, 8 ms of the
/// 2048 Hz one Windows runs. A stall longer than that (a paused VM, a host
/// that stopped this process) is time the guest finds out about from its
/// clock source, not from a burst of interrupts.
pub const MAX_OWED: u32 = 16;

impl std::fmt::Debug for RtcInterrupts {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("RtcInterrupts")
            .field("reg_a", &self.reg_a.load(Ordering::Relaxed))
            .field("reg_b", &self.reg_b.load(Ordering::Relaxed))
            .field("flags", &self.flags.load(Ordering::Relaxed))
            .field("connected", &self.line.get().is_some())
            .finish()
    }
}

impl RtcInterrupts {
    /// Wires the RTC to its interrupt line (IRQ 8). Once; a second call is
    /// ignored, because the line is the machine's wiring, not the guest's.
    pub fn connect(&self, line: Arc<dyn IrqLine>) {
        if self.line.set(line).is_err() {
            tracing::warn!("the RTC interrupt line is already connected");
        }
    }

    /// The periodic interrupt's period, while the guest has it enabled.
    pub fn period(&self) -> Option<Duration> {
        if self.reg_b.load(Ordering::Acquire) & REG_B_PIE == 0 {
            return None;
        }
        let hz = periodic_hz(self.reg_a.load(Ordering::Acquire))?;
        Some(Duration::from_nanos(1_000_000_000 / hz))
    }

    /// How many times the line has been raised.
    pub fn raised(&self) -> u64 {
        self.raised.load(Ordering::Acquire)
    }

    /// One periodic event.
    pub fn periodic_tick(&self) {
        self.periodic_ticks(1);
    }

    /// `count` periodic events came due at once: the host thread slept
    /// through all but the last. One is raised now, if the guest has
    /// acknowledged the one before; the rest stay owed, and each register C
    /// read raises the next ([`MAX_OWED`] at most).
    ///
    /// A guest that takes this interrupt as its clock counts on the *rate*:
    /// Windows runs it at 2048 Hz, and a host thread cannot sleep for 488 µs.
    /// Measured on a Windows host, it delivered 1050 a second, and the
    /// guest's 15.6 ms timer tick took 30 (ADR-0008, the frame-clock
    /// amendment). This is the 8254's policy too ([`crate::irqchip::pit`]).
    pub fn periodic_ticks(&self, count: u32) {
        if self.reg_b.load(Ordering::Acquire) & REG_B_PIE == 0 {
            self.owed.store(0, Ordering::Release);
            self.flags.fetch_or(REG_C_PF, Ordering::AcqRel);
            return;
        }
        let mut owed = self.owed.load(Ordering::Acquire);
        loop {
            let more = owed.saturating_add(count).min(MAX_OWED);
            match self
                .owed
                .compare_exchange_weak(owed, more, Ordering::AcqRel, Ordering::Acquire)
            {
                Ok(_) => break,
                Err(now) => owed = now,
            }
        }
        self.raise_owed();
    }

    /// Raises one owed periodic event, unless an interrupt is still pending
    /// (its acknowledgement will come back here). Called by the timer thread
    /// and by the vCPU that read register C; the `IRQF` bit decides which of
    /// two racing callers raises.
    fn raise_owed(&self) {
        if self.owed.load(Ordering::Acquire) == 0 {
            return;
        }
        let before = self.flags.fetch_or(REG_C_PF | REG_C_IRQF, Ordering::AcqRel);
        if before & REG_C_IRQF != 0 {
            return;
        }
        // Only the caller that found `IRQF` clear gets here, so the count
        // cannot go below zero under it; a rate change may have cleared it.
        let mut owed = self.owed.load(Ordering::Acquire);
        while owed > 0 {
            match self.owed.compare_exchange_weak(
                owed,
                owed - 1,
                Ordering::AcqRel,
                Ordering::Acquire,
            ) {
                Ok(_) => break,
                Err(now) => owed = now,
            }
        }
        self.raise();
    }

    fn raise(&self) {
        if let Some(line) = self.line.get() {
            self.raised.fetch_add(1, Ordering::AcqRel);
            if let Err(error) = line.trigger() {
                tracing::warn!(%error, "could not raise the RTC interrupt");
            }
        }
    }

    /// Periodic events due and not yet raised.
    pub fn owed(&self) -> u32 {
        self.owed.load(Ordering::Acquire)
    }

    /// The once-a-second update cycle finished.
    pub fn update_ended(&self) {
        self.event(REG_C_UF, REG_B_UIE);
    }

    /// Sets `flag`; when its enable is on and no interrupt is already
    /// pending, sets `IRQF` and raises the line.
    fn event(&self, flag: u8, enable: u8) {
        if self.reg_b.load(Ordering::Acquire) & enable == 0 {
            self.flags.fetch_or(flag, Ordering::AcqRel);
            return;
        }
        let before = self.flags.fetch_or(flag | REG_C_IRQF, Ordering::AcqRel);
        if before & REG_C_IRQF != 0 {
            return;
        }
        self.raise();
    }

    /// Register C read: the flags, cleared. It is also the acknowledgement
    /// the next owed periodic event was waiting for.
    fn take_flags(&self) -> u8 {
        let flags = self.flags.swap(0, Ordering::AcqRel);
        self.raise_owed();
        flags
    }

    /// Power-on, or a restored machine: nothing pending and nothing owed.
    fn clear(&self) {
        self.owed.store(0, Ordering::Release);
        self.flags.store(0, Ordering::Release);
    }

    /// Follows the register file after a write, a reset or a restore. What
    /// was owed at the old rate is not owed at the new one.
    fn sync(&self, reg_a: u8, reg_b: u8) {
        let changed = self.reg_a.swap(reg_a, Ordering::AcqRel) != reg_a;
        let was = self.reg_b.swap(reg_b, Ordering::AcqRel);
        if changed || (was ^ reg_b) & REG_B_PIE != 0 {
            self.owed.store(0, Ordering::Release);
        }
    }
}

/// The RTC/CMOS device.
#[derive(Debug)]
pub struct Rtc {
    /// Last value written to the index port, NMI bit included.
    index: u8,
    /// Register file for everything that is plain CMOS RAM.
    cmos: [u8; CMOS_SIZE],
    /// Source of the wall clock, replaceable in tests.
    now: fn() -> i64,
    /// The interrupt half, which outlives a reset: the line is wiring.
    interrupts: Arc<RtcInterrupts>,
}

impl Default for Rtc {
    fn default() -> Self {
        Self::new()
    }
}

impl Rtc {
    pub fn new() -> Self {
        Self::with_clock(unix_now)
    }

    /// Test constructor with a fixed clock.
    pub fn with_clock(now: fn() -> i64) -> Self {
        Self::power_on(now, Arc::new(RtcInterrupts::default()))
    }

    fn power_on(now: fn() -> i64, interrupts: Arc<RtcInterrupts>) -> Self {
        let mut cmos = [0u8; CMOS_SIZE];
        cmos[REG_A as usize] = 0x26; // divider on, 1024 Hz rate; UIP clear
        cmos[REG_B as usize] = DEFAULT_REG_B;
        cmos[REG_D as usize] = REG_D_VRT;
        interrupts.sync(cmos[REG_A as usize], cmos[REG_B as usize]);
        interrupts.clear();
        Self {
            index: 0,
            cmos,
            now,
            interrupts,
        }
    }

    /// The interrupt half, for wiring it and for the timer thread.
    pub fn interrupts(&self) -> &Arc<RtcInterrupts> {
        &self.interrupts
    }

    /// The wall clock this RTC reads, for the timer thread's update cycle.
    pub fn clock(&self) -> fn() -> i64 {
        self.now
    }

    pub fn contains(port: u16) -> bool {
        port == RTC_INDEX_PORT || port == RTC_DATA_PORT
    }

    /// Machine reset (ADR-0005): the index latch and the CMOS register file
    /// back to power-on, with the same clock behind them.
    ///
    /// A real machine's CMOS is battery-backed and survives a reboot, but every
    /// byte of ours is either synthesised from the host clock on read or was
    /// written by the firmware that is about to run again — so a clean slate is
    /// both simpler and closer to what the next boot expects to find.
    pub fn reset(&mut self) {
        *self = Self::power_on(self.now, Arc::clone(&self.interrupts));
    }

    fn register(&self) -> u8 {
        self.index & INDEX_MASK
    }

    fn reg_b(&self) -> u8 {
        self.cmos[REG_B as usize]
    }

    /// Encodes a time field per register B: binary or BCD.
    fn encode(&self, value: u8) -> u8 {
        if self.reg_b() & REG_B_BINARY != 0 {
            value
        } else {
            to_bcd(value)
        }
    }

    /// Encodes the hour, honouring both the encoding and 12/24-hour mode.
    fn encode_hour(&self, hour24: u8) -> u8 {
        if self.reg_b() & REG_B_24H != 0 {
            return self.encode(hour24);
        }
        let pm = hour24 >= 12;
        let hour12 = match hour24 % 12 {
            0 => 12,
            h => h,
        };
        // In 12-hour mode bit 7 of the hour register marks PM, and it sits
        // outside the BCD digits.
        self.encode(hour12) | if pm { 0x80 } else { 0 }
    }

    /// Value the guest reads from the currently selected register.
    fn read_register(&self) -> u8 {
        let time = civil_from_unix((self.now)());
        match self.register() {
            REG_SECONDS => self.encode(time.second),
            REG_MINUTES => self.encode(time.minute),
            REG_HOURS => self.encode_hour(time.hour),
            REG_DAY_OF_WEEK => self.encode(time.weekday),
            REG_DAY_OF_MONTH => self.encode(time.day),
            REG_MONTH => self.encode(time.month),
            REG_YEAR => self.encode((time.year % 100) as u8),
            REG_CENTURY => self.encode((time.year / 100) as u8),
            // UIP (bit 7) is always clear: the time is computed at read time,
            // so there is no update window to be inside of.
            REG_A => self.cmos[REG_A as usize] & 0x7f,
            // Reading it is the acknowledgement; see `io_read`.
            REG_C => 0,
            REG_D => self.cmos[REG_D as usize] | REG_D_VRT,
            other => self.cmos[other as usize],
        }
    }

    /// The index latch and the CMOS bytes (ADR-0006).
    ///
    /// Not the time-of-day registers: those are computed from the host clock on
    /// every read and have no stored form. A restored guest therefore sees the
    /// real wall-clock time, which has moved on — the same thing a laptop's own
    /// suspend does, and the reason `hwclock` exists.
    pub fn save_state(&self) -> crate::state::SavedRtc {
        crate::state::SavedRtc {
            index: self.index,
            cmos: self.cmos.to_vec(),
        }
    }

    /// Puts it back. A snapshot whose CMOS is a different size is refused: it
    /// describes an RTC this build did not make.
    pub fn load_state(
        &mut self,
        state: &crate::state::SavedRtc,
    ) -> Result<(), crate::state::StateError> {
        if state.cmos.len() != self.cmos.len() {
            return Err(crate::state::StateError::Count {
                what: "CMOS bytes",
                snapshot: state.cmos.len(),
                current: self.cmos.len(),
            });
        }
        self.index = state.index;
        self.cmos.copy_from_slice(&state.cmos);
        // Pending flags are not saved: a resumed guest that was waiting for
        // an interrupt gets the next one.
        self.interrupts
            .sync(self.cmos[REG_A as usize], self.cmos[REG_B as usize]);
        self.interrupts.clear();
        Ok(())
    }

    pub fn io_read(&mut self, port: u16, data: &mut [u8]) {
        let value = if port == RTC_INDEX_PORT {
            // EDK2 reads this back to preserve the NMI-disable bit.
            self.index
        } else if self.register() == REG_C {
            // Reading register C clears it, and that is what lets the next
            // event raise the line again.
            self.interrupts.take_flags()
        } else {
            self.read_register()
        };
        // Byte port: a wider access reads the same value, as an 8-bit device
        // on a 16/32-bit bus does for the low lane and floats the rest.
        for (i, byte) in data.iter_mut().enumerate() {
            *byte = if i == 0 { value } else { 0xff };
        }
    }

    pub fn io_write(&mut self, port: u16, data: &[u8]) {
        let Some(&value) = data.first() else {
            return;
        };
        if port == RTC_INDEX_PORT {
            self.index = value;
            return;
        }
        match self.register() {
            // The clock itself is not settable: the guest is untrusted and the
            // host clock is not its to change. Accepted and dropped, which is
            // what the firmware's "set the time" fallback path tolerates.
            REG_SECONDS | REG_MINUTES | REG_HOURS | REG_DAY_OF_WEEK | REG_DAY_OF_MONTH
            | REG_MONTH | REG_YEAR | REG_CENTURY => {
                tracing::trace!(
                    register = format_args!("{:#02x}", self.register()),
                    "dropping guest RTC time write"
                );
            }
            // Register B selects the encoding and 12/24-hour mode, and the
            // interrupts this RTC delivers. `SET` (stop updating) is dropped:
            // the time is the host's, read on demand.
            REG_B => {
                self.cmos[REG_B as usize] = value
                    & (REG_B_DSE | REG_B_24H | REG_B_BINARY | REG_B_UIE | REG_B_AIE | REG_B_PIE);
                self.interrupts
                    .sync(self.cmos[REG_A as usize], self.cmos[REG_B as usize]);
            }
            REG_A => {
                // Rate-select and divider bits are storage; UIP is read-only.
                self.cmos[REG_A as usize] = value & 0x7f;
                self.interrupts
                    .sync(self.cmos[REG_A as usize], self.cmos[REG_B as usize]);
            }
            REG_C => {} // read-only flag register
            REG_D => self.cmos[REG_D as usize] = value | REG_D_VRT,
            other => self.cmos[other as usize] = value,
        }
    }
}

/// Longest the timer sleeps when there is nothing periodic to do: the
/// update-ended event is checked at least this often.
const IDLE_POLL: Duration = Duration::from_millis(10);

/// Shortest sleep, so a guest asking for 8 kHz cannot make the thread spin.
const MIN_SLEEP: Duration = Duration::from_micros(100);

/// How often the timer thread reports what its periodic interrupt did.
const TICK_REPORT: Duration = Duration::from_secs(5);

/// What the periodic interrupt did over one report: a Windows guest's clock
/// *is* this interrupt, so a tick that is late or never taken is time its
/// scheduler, its timers and its compositor's frame clock did not get
/// (`RUST_LOG=machine_x86::rtc=debug`).
struct TickStats {
    since: Instant,
    period: Duration,
    wakes: u64,
    due: u64,
    owed_max: u32,
    late_sum: Duration,
    late_max: Duration,
}

impl TickStats {
    fn new(now: Instant) -> Self {
        Self {
            since: now,
            period: Duration::ZERO,
            wakes: 0,
            due: 0,
            owed_max: 0,
            late_sum: Duration::ZERO,
            late_max: Duration::ZERO,
        }
    }

    /// One wake-up of the thread: how many periodic events came due in it,
    /// how late it was for the first, and how many the guest is still owed.
    fn tick(&mut self, period: Duration, count: u32, late: Duration, owed: u32, now: Instant) {
        if period != self.period {
            tracing::debug!(
                hz = 1_000_000_000 / period.as_nanos().max(1),
                "the guest set the RTC's periodic rate"
            );
            self.report(now);
            self.period = period;
        }
        self.wakes += 1;
        self.due += u64::from(count);
        self.owed_max = self.owed_max.max(owed);
        self.late_sum += late;
        self.late_max = self.late_max.max(late);
        if now.saturating_duration_since(self.since) >= TICK_REPORT {
            self.report(now);
        }
    }

    fn report(&mut self, now: Instant) {
        if self.wakes > 0 {
            let secs = now.saturating_duration_since(self.since).as_secs_f64();
            tracing::debug!(
                hz = 1_000_000_000 / self.period.as_nanos().max(1),
                due_per_s = format_args!("{:.1}", self.due as f64 / secs.max(1e-9)),
                wakes_per_s = format_args!("{:.1}", self.wakes as f64 / secs.max(1e-9)),
                owed_max = self.owed_max,
                late_mean_us = (self.late_sum.as_micros() / u128::from(self.wakes)) as u64,
                late_max_us = self.late_max.as_micros() as u64,
                "RTC periodic interrupt"
            );
        }
        *self = Self {
            period: self.period,
            ..Self::new(now)
        };
    }
}

/// The host thread that makes the RTC's events happen: the periodic
/// interrupt at the rate register A selects, and the update-ended event once
/// a second. Joined on drop, so it cannot outlive the line it raises.
///
/// The thread sleeps, and a sleep is a millisecond long at best while the
/// guest may ask for 2048 or 8192 events a second: every period that ended
/// during a sleep is handed over at once ([`RtcInterrupts::periodic_ticks`]),
/// and the guest's acknowledgements pull the owed ones in. An interrupt the
/// guest has not acknowledged yet (`IRQF` still set) is never raised twice.
pub struct RtcTimer {
    stop: Arc<AtomicBool>,
    handle: Option<JoinHandle<()>>,
}

impl RtcTimer {
    pub fn start(interrupts: Arc<RtcInterrupts>, now: fn() -> i64) -> std::io::Result<Self> {
        let stop = Arc::new(AtomicBool::new(false));
        let flag = Arc::clone(&stop);
        let handle = std::thread::Builder::new()
            .name("rtc".into())
            .spawn(move || {
                crate::irqchip::raise_timer_thread_priority();
                let mut second = now();
                let mut due: Option<Instant> = None;
                let mut stats = TickStats::new(Instant::now());
                while !flag.load(Ordering::Acquire) {
                    let current = now();
                    if current != second {
                        second = current;
                        interrupts.update_ended();
                    }
                    let Some(period) = interrupts.period() else {
                        due = None;
                        std::thread::sleep(IDLE_POLL);
                        continue;
                    };
                    let at = Instant::now();
                    let next = *due.get_or_insert(at + period);
                    if at >= next {
                        // Every period that ended while this thread slept,
                        // not only the last: the guest counts on the rate.
                        let late = at - next;
                        let missed = (late.as_nanos() / period.as_nanos().max(1))
                            .min(u128::from(MAX_OWED)) as u32;
                        let count = missed + 1;
                        interrupts.periodic_ticks(count);
                        stats.tick(period, count, late, interrupts.owed(), at);
                        let after = next + period * count;
                        due = Some(if after < at { at + period } else { after });
                        continue;
                    }
                    std::thread::sleep((next - at).clamp(MIN_SLEEP, IDLE_POLL));
                }
            })?;
        Ok(Self {
            stop,
            handle: Some(handle),
        })
    }
}

impl Drop for RtcTimer {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::Release);
        if let Some(handle) = self.handle.take() {
            let _ = handle.join();
        }
    }
}

fn unix_now() -> i64 {
    match SystemTime::now().duration_since(UNIX_EPOCH) {
        Ok(d) => d.as_secs() as i64,
        // Before 1970: the host clock is nonsense, but the guest still needs a
        // valid-looking date rather than a panic.
        Err(e) => -(e.duration().as_secs() as i64),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// 2026-08-19T01:02:03Z — a Wednesday.
    const FIXED: i64 = 1_787_101_323;

    fn fixed_clock() -> i64 {
        FIXED
    }

    fn rtc() -> Rtc {
        Rtc::with_clock(fixed_clock)
    }

    /// Reads a register the way EDK2's `IoRtcRead()` does: read the index port,
    /// OR in the register number keeping bit 7, write it back, read data.
    fn edk2_read(rtc: &mut Rtc, register: u8) -> u8 {
        let mut index = [0u8; 1];
        rtc.io_read(RTC_INDEX_PORT, &mut index);
        rtc.io_write(RTC_INDEX_PORT, &[register | (index[0] & NMI_DISABLE)]);
        let mut data = [0u8; 1];
        rtc.io_read(RTC_DATA_PORT, &mut data);
        data[0]
    }

    #[test]
    fn civil_conversion_matches_known_dates() {
        let epoch = civil_from_unix(0);
        assert_eq!((epoch.year, epoch.month, epoch.day), (1970, 1, 1));
        assert_eq!(epoch.weekday, 5, "1970-01-01 was a Thursday (Sunday = 1)");
        assert_eq!((epoch.hour, epoch.minute, epoch.second), (0, 0, 0));

        let t = civil_from_unix(FIXED);
        assert_eq!((t.year, t.month, t.day), (2026, 8, 19));
        assert_eq!((t.hour, t.minute, t.second), (1, 2, 3));
        assert_eq!(t.weekday, 4, "2026-08-19 is a Wednesday");

        // Leap day, and the day after a century non-leap-year boundary.
        let leap = civil_from_unix(951_782_400); // 2000-02-29T00:00:00Z
        assert_eq!((leap.year, leap.month, leap.day), (2000, 2, 29));
        let mar = civil_from_unix(1_583_020_800); // 2020-03-01T00:00:00Z
        assert_eq!((mar.year, mar.month, mar.day), (2020, 3, 1));
        // Pre-epoch times must not panic or wrap.
        let old = civil_from_unix(-1);
        assert_eq!((old.year, old.month, old.day), (1969, 12, 31));
        assert_eq!((old.hour, old.minute, old.second), (23, 59, 59));
    }

    /// The exact read sequence PcRtcInit performs, in BCD 24-hour mode.
    #[test]
    fn reports_a_valid_bcd_time_to_edk2() {
        let mut rtc = rtc();
        assert_eq!(edk2_read(&mut rtc, REG_B), DEFAULT_REG_B, "24-hour BCD");
        assert_eq!(edk2_read(&mut rtc, REG_SECONDS), 0x03);
        assert_eq!(edk2_read(&mut rtc, REG_MINUTES), 0x02);
        assert_eq!(edk2_read(&mut rtc, REG_HOURS), 0x01);
        assert_eq!(edk2_read(&mut rtc, REG_DAY_OF_MONTH), 0x19);
        assert_eq!(edk2_read(&mut rtc, REG_MONTH), 0x08);
        assert_eq!(edk2_read(&mut rtc, REG_YEAR), 0x26);
        assert_eq!(edk2_read(&mut rtc, REG_CENTURY), 0x20);
    }

    /// `RtcWaitToUpdate()` loops until UIP is clear *and* VRT is set; both are
    /// the difference between a working clock and EFI_DEVICE_ERROR.
    #[test]
    fn uip_is_always_clear_and_vrt_always_set() {
        let mut rtc = rtc();
        assert_eq!(edk2_read(&mut rtc, REG_A) & 0x80, 0, "UIP must be clear");
        assert_ne!(edk2_read(&mut rtc, REG_D) & REG_D_VRT, 0, "VRT must be set");
        // Even if the guest tries to clear VRT.
        rtc.io_write(RTC_INDEX_PORT, &[REG_D]);
        rtc.io_write(RTC_DATA_PORT, &[0x00]);
        assert_ne!(edk2_read(&mut rtc, REG_D) & REG_D_VRT, 0);
        // Or set UIP.
        rtc.io_write(RTC_INDEX_PORT, &[REG_A]);
        rtc.io_write(RTC_DATA_PORT, &[0xff]);
        assert_eq!(edk2_read(&mut rtc, REG_A) & 0x80, 0);
    }

    /// The index port must read back, or EDK2's `Address | (IoRead8 (index) &
    /// 0x80)` corrupts every register number.
    #[test]
    fn index_port_reads_back_and_preserves_the_nmi_bit() {
        let mut rtc = rtc();
        rtc.io_write(RTC_INDEX_PORT, &[NMI_DISABLE | REG_YEAR]);
        let mut index = [0u8; 1];
        rtc.io_read(RTC_INDEX_PORT, &mut index);
        assert_eq!(index[0], NMI_DISABLE | REG_YEAR);
        // With NMI disabled, the EDK2 sequence still selects the right register.
        assert_eq!(edk2_read(&mut rtc, REG_MONTH), 0x08);
        rtc.io_read(RTC_INDEX_PORT, &mut index);
        assert_eq!(index[0] & NMI_DISABLE, NMI_DISABLE, "NMI bit must survive");
    }

    #[test]
    fn binary_and_twelve_hour_modes_are_honoured() {
        let mut rtc = rtc();
        // Binary, 24-hour.
        rtc.io_write(RTC_INDEX_PORT, &[REG_B]);
        rtc.io_write(RTC_DATA_PORT, &[REG_B_24H | REG_B_BINARY]);
        assert_eq!(edk2_read(&mut rtc, REG_YEAR), 26);
        assert_eq!(edk2_read(&mut rtc, REG_DAY_OF_MONTH), 19);

        // BCD, 12-hour: 01:02 is AM, so no PM bit.
        rtc.io_write(RTC_INDEX_PORT, &[REG_B]);
        rtc.io_write(RTC_DATA_PORT, &[0]);
        assert_eq!(edk2_read(&mut rtc, REG_HOURS), 0x01);

        // 13:00 in 12-hour BCD is 0x01 with the PM bit.
        let mut afternoon = Rtc::with_clock(|| 1_787_144_400); // 2026-08-19T13:00:00Z
        afternoon.io_write(RTC_INDEX_PORT, &[REG_B]);
        afternoon.io_write(RTC_DATA_PORT, &[0]);
        assert_eq!(edk2_read(&mut afternoon, REG_HOURS), 0x81);
        // And 0x13 in 24-hour BCD.
        afternoon.io_write(RTC_INDEX_PORT, &[REG_B]);
        afternoon.io_write(RTC_DATA_PORT, &[REG_B_24H]);
        assert_eq!(edk2_read(&mut afternoon, REG_HOURS), 0x13);
    }

    #[test]
    fn guest_cannot_move_the_clock_but_can_use_cmos_ram() {
        let mut rtc = rtc();
        rtc.io_write(RTC_INDEX_PORT, &[REG_YEAR]);
        rtc.io_write(RTC_DATA_PORT, &[0x99]);
        assert_eq!(edk2_read(&mut rtc, REG_YEAR), 0x26, "clock must not move");

        // Scratch RAM above the register block is ordinary storage.
        for reg in [0x0e, 0x20, 0x7f] {
            rtc.io_write(RTC_INDEX_PORT, &[reg]);
            rtc.io_write(RTC_DATA_PORT, &[0x5a]);
            assert_eq!(edk2_read(&mut rtc, reg), 0x5a, "CMOS RAM {reg:#x}");
        }
    }

    /// A real MC146818 advances; a stuck clock breaks firmware stall loops.
    #[test]
    fn clock_follows_the_host() {
        let mut early = Rtc::with_clock(|| FIXED);
        let mut late = Rtc::with_clock(|| FIXED + 3600);
        assert_eq!(edk2_read(&mut early, REG_HOURS), 0x01);
        assert_eq!(edk2_read(&mut late, REG_HOURS), 0x02);
    }

    #[derive(Default)]
    struct Line(AtomicU64);

    impl IrqLine for Line {
        fn trigger(&self) -> Result<(), virtio_core::interrupt::InterruptError> {
            self.0.fetch_add(1, Ordering::AcqRel);
            Ok(())
        }
    }

    fn write_reg(rtc: &mut Rtc, register: u8, value: u8) {
        rtc.io_write(RTC_INDEX_PORT, &[register]);
        rtc.io_write(RTC_DATA_PORT, &[value]);
    }

    fn read_reg(rtc: &mut Rtc, register: u8) -> u8 {
        rtc.io_write(RTC_INDEX_PORT, &[register]);
        let mut value = [0u8];
        rtc.io_read(RTC_DATA_PORT, &mut value);
        value[0]
    }

    #[test]
    fn rate_select_follows_the_mc146818_table() {
        assert_eq!(periodic_hz(0x26), Some(1024));
        assert_eq!(periodic_hz(0x25), Some(2048));
        assert_eq!(periodic_hz(0x23), Some(8192));
        assert_eq!(periodic_hz(0x21), Some(256));
        assert_eq!(periodic_hz(0x2f), Some(2));
        assert_eq!(periodic_hz(0x20), None, "rate 0 is off");
        assert_eq!(periodic_hz(0x06), None, "divider held in reset");
    }

    /// What Windows' HAL does (ADR-0008 §3): rate, PIE, then wait for IRQ 8
    /// and read register C in the handler.
    #[test]
    fn the_periodic_interrupt_raises_once_until_register_c_is_read() {
        let mut rtc = rtc();
        let line = Arc::new(Line::default());
        rtc.interrupts().connect(line.clone());
        write_reg(&mut rtc, REG_A, 0x25);
        assert_eq!(rtc.interrupts().period(), None, "PIE still off");
        write_reg(&mut rtc, REG_B, REG_B_24H | REG_B_PIE);
        assert_eq!(
            rtc.interrupts().period(),
            Some(Duration::from_nanos(1_000_000_000 / 2048))
        );
        rtc.interrupts().periodic_tick();
        assert_eq!(line.0.load(Ordering::Acquire), 1);
        assert_eq!(read_reg(&mut rtc, REG_C), REG_C_IRQF | REG_C_PF);
        assert_eq!(read_reg(&mut rtc, REG_C), 0, "reading clears it");
        rtc.interrupts().periodic_tick();
        assert_eq!(line.0.load(Ordering::Acquire), 2);
    }

    /// The guest counts on the rate: an event that comes due while the one
    /// before is unacknowledged is owed, not lost, and the acknowledgement
    /// (the register C read) is what raises it.
    #[test]
    fn an_unacknowledged_interrupt_leaves_the_next_one_owed() {
        let mut rtc = rtc();
        let line = Arc::new(Line::default());
        rtc.interrupts().connect(line.clone());
        write_reg(&mut rtc, REG_A, 0x25);
        write_reg(&mut rtc, REG_B, REG_B_24H | REG_B_PIE);
        rtc.interrupts().periodic_tick();
        rtc.interrupts().periodic_tick();
        assert_eq!(line.0.load(Ordering::Acquire), 1, "IRQF still pending");
        assert_eq!(rtc.interrupts().owed(), 1);
        assert_eq!(read_reg(&mut rtc, REG_C), REG_C_IRQF | REG_C_PF);
        assert_eq!(line.0.load(Ordering::Acquire), 2, "the read raised it");
        assert_eq!(rtc.interrupts().owed(), 0);
        assert_eq!(read_reg(&mut rtc, REG_C), REG_C_IRQF | REG_C_PF);
        assert_eq!(read_reg(&mut rtc, REG_C), 0, "nothing more is owed");
        assert_eq!(line.0.load(Ordering::Acquire), 2);
    }

    /// A host thread that slept through several periods hands them all over,
    /// and the guest's acknowledgements pull them in one by one.
    #[test]
    fn a_late_wake_up_delivers_every_period_it_slept_through() {
        let mut rtc = rtc();
        let line = Arc::new(Line::default());
        rtc.interrupts().connect(line.clone());
        write_reg(&mut rtc, REG_A, 0x25);
        write_reg(&mut rtc, REG_B, REG_B_24H | REG_B_PIE);
        rtc.interrupts().periodic_ticks(3);
        assert_eq!(line.0.load(Ordering::Acquire), 1);
        let mut taken = 0;
        while read_reg(&mut rtc, REG_C) & REG_C_PF != 0 {
            taken += 1;
            assert!(taken <= 3, "more interrupts than periods");
        }
        assert_eq!(taken, 3);
        assert_eq!(line.0.load(Ordering::Acquire), 3);
    }

    #[test]
    fn what_is_owed_is_bounded_and_dropped_with_the_rate() {
        let mut rtc = rtc();
        let line = Arc::new(Line::default());
        rtc.interrupts().connect(line.clone());
        write_reg(&mut rtc, REG_A, 0x25);
        write_reg(&mut rtc, REG_B, REG_B_24H | REG_B_PIE);
        // A stalled guest: a second's worth of periods, none acknowledged.
        rtc.interrupts().periodic_ticks(2048);
        rtc.interrupts().periodic_ticks(2048);
        assert_eq!(rtc.interrupts().owed(), MAX_OWED);
        assert_eq!(line.0.load(Ordering::Acquire), 1);
        // A new rate owes nothing from the old one.
        write_reg(&mut rtc, REG_A, 0x26);
        assert_eq!(rtc.interrupts().owed(), 0);
        rtc.interrupts().periodic_ticks(4);
        // Turning the interrupt off drops the rest, and the read that follows
        // raises nothing.
        write_reg(&mut rtc, REG_B, REG_B_24H);
        assert_eq!(rtc.interrupts().owed(), 0);
        let raised = line.0.load(Ordering::Acquire);
        read_reg(&mut rtc, REG_C);
        rtc.interrupts().periodic_ticks(4);
        assert_eq!(line.0.load(Ordering::Acquire), raised);
        assert_eq!(rtc.interrupts().owed(), 0);
    }

    #[test]
    fn a_reset_owes_nothing() {
        let mut rtc = rtc();
        let line = Arc::new(Line::default());
        rtc.interrupts().connect(line.clone());
        write_reg(&mut rtc, REG_A, 0x25);
        write_reg(&mut rtc, REG_B, REG_B_24H | REG_B_PIE);
        rtc.interrupts().periodic_ticks(5);
        rtc.reset();
        assert_eq!(rtc.interrupts().owed(), 0);
        assert_eq!(read_reg(&mut rtc, REG_C), 0);
        assert_eq!(line.0.load(Ordering::Acquire), 1);
    }

    #[test]
    fn a_disabled_event_sets_its_flag_but_no_interrupt() {
        let mut rtc = rtc();
        let line = Arc::new(Line::default());
        rtc.interrupts().connect(line.clone());
        rtc.interrupts().update_ended();
        assert_eq!(line.0.load(Ordering::Acquire), 0);
        assert_eq!(read_reg(&mut rtc, REG_C), REG_C_UF);
        write_reg(&mut rtc, REG_B, REG_B_24H | REG_B_UIE);
        rtc.interrupts().update_ended();
        assert_eq!(line.0.load(Ordering::Acquire), 1);
        assert_eq!(read_reg(&mut rtc, REG_C), REG_C_IRQF | REG_C_UF);
    }

    #[test]
    fn reset_disables_interrupts_but_keeps_the_wiring() {
        let mut rtc = rtc();
        let line = Arc::new(Line::default());
        rtc.interrupts().connect(line.clone());
        write_reg(&mut rtc, REG_B, REG_B_24H | REG_B_PIE);
        rtc.interrupts().periodic_tick();
        rtc.reset();
        assert_eq!(rtc.interrupts().period(), None);
        assert_eq!(read_reg(&mut rtc, REG_C), 0, "pending flags cleared");
        write_reg(&mut rtc, REG_B, REG_B_24H | REG_B_PIE);
        rtc.interrupts().periodic_tick();
        assert_eq!(line.0.load(Ordering::Acquire), 2, "same line after reset");
    }

    #[test]
    fn the_timer_thread_delivers_periodic_interrupts() {
        let mut rtc = Rtc::new();
        let line = Arc::new(Line::default());
        rtc.interrupts().connect(line.clone());
        write_reg(&mut rtc, REG_A, 0x26);
        write_reg(&mut rtc, REG_B, REG_B_24H | REG_B_PIE);
        let timer = RtcTimer::start(Arc::clone(rtc.interrupts()), rtc.clock()).unwrap();
        let deadline = Instant::now() + Duration::from_secs(5);
        let mut acknowledged = 0;
        while acknowledged < 3 && Instant::now() < deadline {
            if read_reg(&mut rtc, REG_C) & REG_C_PF != 0 {
                acknowledged += 1;
            }
            std::thread::sleep(Duration::from_millis(1));
        }
        drop(timer);
        assert!(acknowledged >= 3, "saw {acknowledged} periodic events");
        assert!(line.0.load(Ordering::Acquire) >= 3);
    }

    #[test]
    fn only_the_two_cmos_ports_are_claimed() {
        assert!(Rtc::contains(0x70));
        assert!(Rtc::contains(0x71));
        for port in [0x6f, 0x72, 0x3f8, 0x608, 0xcf8] {
            assert!(!Rtc::contains(port), "port {port:#x}");
        }
    }
}
