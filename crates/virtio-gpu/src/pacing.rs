//! Frame-pacing statistics for the scanout path (ADR-0004 phase 2's
//! measurement requirement, extended for GAME-2105).
//!
//! The device is the only place in the system that sees *every* frame the
//! guest presents: a compositor flush is a `RESOURCE_FLUSH` on the scanout
//! resource. Recording the interval between those flushes gives a host-side
//! frame clock for free — no guest agent, no instrumented mutter, and it
//! works the same on a 2D device, a synchronous-fence virgl device and an
//! asynchronous-fence one, which is exactly what a before/after needs.
//!
//! # The interval is not one number, it is three
//!
//! Phase 2 measured a GNOME guest at exactly 30 fps with a device that spent
//! 1.9 ms of the 33.4 ms serving it, and could say nothing about the other
//! 31.5 ms. So an interval is now decomposed at the one place that can see
//! the boundaries — the control queue:
//!
//! ```text
//!  present N-1 answered                                    present N answered
//!          │                                                        │
//!          ├── quiet ──┬─────── submit ───────┬───── service ───────┤
//!          │           │                      │                     │
//!          │      first command          RESOURCE_FLUSH        readback +
//!          │      of frame N             of frame N            sink push
//! ```
//!
//! * **quiet** — the guest asked the device for nothing at all. A guest that
//!   is waiting on a clock of its own (a compositor's frame scheduler) spends
//!   its frame here; a guest that is working does not.
//! * **submit** — the guest was feeding this device: transfers, 3D submits,
//!   and finally the flush. Long here means guest-side rendering cost, or a
//!   device round trip the guest serializes on.
//! * **service** — the device's own cost, phase 2's number.
//!
//! The three sum to the interval exactly, which is what makes the attribution
//! an argument rather than a guess.
//!
//! # Percentiles, drops and duplicates
//!
//! A mean hides everything that makes a desktop feel broken, so the report
//! also carries the classic pair of tail figures — the mean of the slowest
//! 1 % and 0.1 % of intervals, quoted as fps — two jitter figures (the
//! intervals' standard deviation, and the mean change from one interval to
//! the next), and two counters defined against the refresh period the EDID
//! advertises ([`FramePacing::set_refresh`], [`REFRESH_PERIOD`] until then):
//!
//! * **duplicate** — refresh slots in which the guest presented nothing, so
//!   the host has to show the previous image again. A guest at half the
//!   refresh rate produces exactly one duplicate per frame.
//! * **dropped** — presents that landed inside the same refresh slot as the
//!   one before, so the earlier image was replaced before any display could
//!   have shown it. Work the guest did for nobody.
//!
//! Both are the *guest's* view: a virtual monitor refreshing at the advertised
//! rate. Whether the host's own monitor shows each frame is the window's
//! business, and its statistics count it (`display statistics`: `guest_fps`
//! against `fps`, and `unshown_per_s`) — a guest told 240 Hz on a 60 Hz host
//! monitor reports no drops here and has three frames in four never shown.
//!
//! # Everything that depends on the refresh follows it
//!
//! A frame is `late` past 1.2 refresh periods (20 ms at 60 Hz, [`FRAME_BUDGET`];
//! 5 ms at 240 Hz), and a report covers two seconds of refresh slots
//! ([`REPORT_EVERY`] frames at 60 Hz, 480 at 240 Hz). Before the refresh became
//! a profile key both were 60 Hz constants, which at 240 Hz reported every
//! half second and called a frame that missed three slots on time.
//!
//! Portable, allocation-free on the hot path and unit-tested; the device logs
//! a report every [`REPORT_EVERY`] frames at `info`, and `--frame-stats`
//! additionally keeps a JSON file up to date so runs can be compared with a
//! diff instead of an impression.

use std::path::Path;
use std::time::{Duration, Instant};

/// Frames between reports at the default 60 Hz: one report every two seconds
/// — often enough to see a phase change (GDM → session → an app starting),
/// rare enough to keep the log readable. A faster advertised refresh keeps
/// the two seconds ([`report_every`]); a slower one keeps this floor.
pub const REPORT_EVERY: u64 = 120;

/// How long a report window lasts, in refresh slots' time.
const REPORT_SECONDS: u64 = 2;

/// An interval longer than this counts as `late` at the default 60 Hz. 20 ms
/// is 1.2 periods: a compositor targeting 60 Hz that misses this missed its
/// deadline. At another refresh the budget is the same 1.2 periods
/// ([`late_budget_us`]).
pub const FRAME_BUDGET: Duration = Duration::from_millis(20);

/// Presents per report for a refresh period: two seconds of slots, and never
/// fewer than [`REPORT_EVERY`].
pub fn report_every(refresh: Duration) -> u64 {
    let period = refresh.as_nanos();
    if period == 0 {
        return REPORT_EVERY;
    }
    // Rounded: 2 s of a 120 Hz period (8 333 333 ns) is 240 slots, not 241.
    let slots = (u128::from(REPORT_SECONDS) * 1_000_000_000 + period / 2) / period;
    u64::try_from(slots).unwrap_or(u64::MAX).max(REPORT_EVERY)
}

/// The `late` threshold for a refresh period, in microseconds: 1.2 periods,
/// which is [`FRAME_BUDGET`] at 60 Hz. A zero period falls back to it.
pub fn late_budget_us(refresh_us: u64) -> u64 {
    if refresh_us == 0 {
        return micros(FRAME_BUDGET);
    }
    refresh_us.saturating_mul(6) / 5
}

/// Intervals longer than this are not frame pacing at all — an idle desktop
/// that presents nothing for a second, a session switch, a guest that was
/// suspended — and would poison min/mean/late. They are counted separately.
pub const IDLE_GAP: Duration = Duration::from_millis(500);

/// The refresh period a new [`FramePacing`] counts against: the default
/// 60 Hz of [`crate::edid::DEFAULT_REFRESH_HZ`]. The device replaces it with
/// the advertised one (`[display] refresh_hz`, [`FramePacing::set_refresh`]).
pub const REFRESH_PERIOD: Duration = Duration::from_nanos(16_666_667);

/// Histogram bucket width. 100 µs is a hundredth of a 60 Hz frame and a
/// fortieth of a 240 Hz one — finer than any conclusion drawn from these
/// numbers.
const BUCKET_US: u64 = 100;

/// Buckets before the overflow bin: 0…200 ms, which covers everything short
/// of [`IDLE_GAP`].
const BUCKETS: usize = 2_000;

/// Parts per million of the samples in the "1 % low" figure.
const LOW_1_PPM: u64 = 10_000;
/// Parts per million of the samples in the "0.1 % low" figure.
const LOW_01_PPM: u64 = 1_000;

/// A bucketed distribution of frame intervals, kept so the tail figures do
/// not need every sample retained.
#[derive(Debug, Clone)]
struct Histogram {
    buckets: Vec<u32>,
    /// Samples past [`BUCKETS`] × [`BUCKET_US`], summed rather than bucketed.
    over: u64,
    over_sum_us: u64,
    count: u64,
}

impl Default for Histogram {
    fn default() -> Self {
        Self {
            buckets: vec![0; BUCKETS],
            over: 0,
            over_sum_us: 0,
            count: 0,
        }
    }
}

impl Histogram {
    fn record(&mut self, us: u64) {
        self.count = self.count.saturating_add(1);
        let index = (us / BUCKET_US) as usize;
        match self.buckets.get_mut(index) {
            Some(bucket) => *bucket = bucket.saturating_add(1),
            None => {
                self.over = self.over.saturating_add(1);
                self.over_sum_us = self.over_sum_us.saturating_add(us);
            }
        }
    }

    fn clear(&mut self) {
        self.buckets.fill(0);
        self.over = 0;
        self.over_sum_us = 0;
        self.count = 0;
    }

    /// Mean of the slowest `ppm` parts-per-million of the samples — the "1 %
    /// low" convention, which averages the tail instead of quoting a single
    /// percentile and so does not swing on one outlier.
    ///
    /// Always covers at least one sample, so a short window still reports its
    /// worst frame rather than nothing.
    fn slowest_mean_us(&self, ppm: u64) -> u64 {
        if self.count == 0 {
            return 0;
        }
        let want = (self.count.saturating_mul(ppm) / 1_000_000).max(1);
        let mut need = want;
        let mut sum = 0u64;
        if self.over > 0 {
            let take = need.min(self.over);
            // The overflow bin keeps a true sum, so its samples average
            // exactly; taking a prefix of them uses that average.
            sum = sum.saturating_add(self.over_sum_us / self.over * take);
            need -= take;
        }
        for (index, count) in self.buckets.iter().enumerate().rev() {
            if need == 0 {
                break;
            }
            let count = u64::from(*count);
            if count == 0 {
                continue;
            }
            let take = need.min(count);
            let mid = index as u64 * BUCKET_US + BUCKET_US / 2;
            sum = sum.saturating_add(mid * take);
            need -= take;
        }
        let taken = want - need;
        if taken == 0 {
            return 0;
        }
        sum / taken
    }
}

/// One report's worth of frame statistics.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct PacingReport {
    /// Intervals in this window (frames − 1 for the first window).
    pub intervals: u64,
    pub mean_us: u64,
    pub min_us: u64,
    pub max_us: u64,
    /// Mean of the slowest 1 % of intervals.
    pub low_1_us: u64,
    /// Mean of the slowest 0.1 % of intervals.
    pub low_01_us: u64,
    /// Standard deviation of the intervals: how far a frame strays from
    /// the window's mean. A guest paced by its refresh reads tens of
    /// microseconds; one alternating between two cadences reads half their
    /// difference.
    pub stddev_us: u64,
    /// Mean change from one interval to the next, |Δn − Δn−1|: the judder a
    /// viewer sees. A guest at a steady half rate has none, however late.
    pub jitter_us: u64,
    /// Intervals over 1.2 refresh periods ([`late_budget_us`]).
    pub late: u64,
    /// Gaps over [`IDLE_GAP`], excluded from the statistics above.
    pub idle_gaps: u64,
    /// Refresh slots the guest presented nothing into (the host repeats the
    /// previous image).
    pub duplicate: u64,
    /// Presents superseded inside a single refresh slot (never displayable).
    pub dropped: u64,
    /// Mean time the guest asked this device for *nothing* between one
    /// present being answered and the next frame's first command.
    pub quiet_mean_us: u64,
    pub quiet_max_us: u64,
    /// Mean time between the frame's first command and its `RESOURCE_FLUSH`.
    pub submit_mean_us: u64,
    /// Mean control-queue commands per frame.
    pub commands_mean: u64,
    /// Mean pixels in the flushed rect — the size of the readback the device
    /// had to do. A compositor damaging a menu and one damaging the whole
    /// 1920×1080 screen produce the same frame interval and wildly different
    /// service times; this is the number that tells them apart.
    pub pixels_mean: u64,
    /// Mean time the *device* spent serving a flush — the readback out of the
    /// renderer plus the push into the scanout sink — in microseconds.
    ///
    /// This is the attribution the frame interval alone cannot give: an
    /// interval of 33 ms with a 2 ms service time is a guest that paces
    /// itself, while 33 ms with a 20 ms service time is a host presentation
    /// path that cannot keep up.
    pub service_mean_us: u64,
    pub service_max_us: u64,
    /// Flushes of the window the device served through the **shared** path
    /// (ADR-0004, zero-copy presentation): the display copied the frame on
    /// the GPU instead of the renderer reading it back. The rest went through
    /// the copy path, so `service` is the shared path's cost exactly when this
    /// equals the frames of the window.
    pub shared: u64,
}

/// Microseconds as frames per second, rounded to one decimal.
fn fps_of(us: u64) -> f64 {
    if us == 0 {
        return 0.0;
    }
    (1_000_000.0 / us as f64 * 10.0).round() / 10.0
}

impl PacingReport {
    /// Mean cadence as frames per second, rounded to one decimal — the figure
    /// a human reads first.
    pub fn fps(&self) -> f64 {
        fps_of(self.mean_us)
    }

    /// The 1 % low, quoted the way a frame-rate tool quotes it.
    pub fn low_1_fps(&self) -> f64 {
        fps_of(self.low_1_us)
    }

    /// The 0.1 % low, quoted the way a frame-rate tool quotes it.
    pub fn low_01_fps(&self) -> f64 {
        fps_of(self.low_01_us)
    }

    /// The report as a JSON object body (no surrounding braces), so a caller
    /// can nest it. Hand-written because this crate has no serializer and
    /// does not need one for sixteen integers.
    fn write_json(&self, out: &mut String) {
        use std::fmt::Write;
        // Every write! target here is a String, whose Write impl is infallible.
        let _ = write!(
            out,
            "\"intervals\":{},\"fps\":{:.1},\"mean_us\":{},\"min_us\":{},\"max_us\":{},\
             \"low_1_fps\":{:.1},\"low_1_us\":{},\"low_01_fps\":{:.1},\"low_01_us\":{},\
             \"stddev_us\":{},\"jitter_us\":{},\
             \"late\":{},\"idle_gaps\":{},\"duplicate\":{},\"dropped\":{},\
             \"quiet_mean_us\":{},\"quiet_max_us\":{},\"submit_mean_us\":{},\
             \"commands_mean\":{},\"pixels_mean\":{},\"service_mean_us\":{},\"service_max_us\":{},\
             \"shared\":{}",
            self.intervals,
            self.fps(),
            self.mean_us,
            self.min_us,
            self.max_us,
            self.low_1_fps(),
            self.low_1_us,
            self.low_01_fps(),
            self.low_01_us,
            self.stddev_us,
            self.jitter_us,
            self.late,
            self.idle_gaps,
            self.duplicate,
            self.dropped,
            self.quiet_mean_us,
            self.quiet_max_us,
            self.submit_mean_us,
            self.commands_mean,
            self.pixels_mean,
            self.service_mean_us,
            self.service_max_us,
            self.shared,
        );
    }
}

/// The accumulator behind one report: a window's worth, or a whole run's.
#[derive(Debug, Clone, Default)]
struct Stats {
    hist: Histogram,
    intervals: u64,
    sum_us: u64,
    min_us: u64,
    max_us: u64,
    /// Sum of squared intervals (µs²), for the standard deviation.
    sum_sq: u128,
    /// The window's previous interval, and the summed change between
    /// consecutive ones.
    prev_us: Option<u64>,
    delta_sum_us: u64,
    deltas: u64,
    late: u64,
    idle_gaps: u64,
    duplicate: u64,
    dropped: u64,
    /// Frames whose phase decomposition was complete.
    phases: u64,
    quiet_sum_us: u64,
    quiet_max_us: u64,
    submit_sum_us: u64,
    commands: u64,
    service_count: u64,
    service_sum_us: u64,
    service_max_us: u64,
    pixels: u64,
    /// Flushes served through the shared path.
    shared: u64,
}

impl Stats {
    fn record_interval(&mut self, us: u64, refresh_us: u64) {
        self.intervals = self.intervals.saturating_add(1);
        self.sum_us = self.sum_us.saturating_add(us);
        self.min_us = if self.intervals == 1 {
            us
        } else {
            self.min_us.min(us)
        };
        self.max_us = self.max_us.max(us);
        self.sum_sq = self.sum_sq.saturating_add(u128::from(us) * u128::from(us));
        if let Some(prev) = self.prev_us.replace(us) {
            self.delta_sum_us = self.delta_sum_us.saturating_add(prev.abs_diff(us));
            self.deltas = self.deltas.saturating_add(1);
        }
        self.hist.record(us);
        if us > late_budget_us(refresh_us) {
            self.late = self.late.saturating_add(1);
        }
        // Rounded, so a 16.6 ms interval is one slot and a 33.3 ms one is
        // two — the boundary case is a frame that arrived half a slot late,
        // which is a judgement call either way. A zero refresh period
        // disables both counters rather than dividing by it.
        if let Some(slots) = (us + refresh_us / 2).checked_div(refresh_us) {
            match slots {
                0 => self.dropped = self.dropped.saturating_add(1),
                n => self.duplicate = self.duplicate.saturating_add(n - 1),
            }
        }
    }

    fn record_phases(&mut self, quiet_us: u64, submit_us: u64, commands: u64) {
        self.phases = self.phases.saturating_add(1);
        self.quiet_sum_us = self.quiet_sum_us.saturating_add(quiet_us);
        self.quiet_max_us = self.quiet_max_us.max(quiet_us);
        self.submit_sum_us = self.submit_sum_us.saturating_add(submit_us);
        self.commands = self.commands.saturating_add(commands);
    }

    fn record_service(&mut self, us: u64, pixels: u64) {
        self.service_count = self.service_count.saturating_add(1);
        self.service_sum_us = self.service_sum_us.saturating_add(us);
        self.service_max_us = self.service_max_us.max(us);
        self.pixels = self.pixels.saturating_add(pixels);
    }

    /// Resets everything but keeps the histogram's allocation.
    fn clear(&mut self) {
        self.hist.clear();
        self.intervals = 0;
        self.sum_us = 0;
        self.min_us = 0;
        self.max_us = 0;
        self.sum_sq = 0;
        self.prev_us = None;
        self.delta_sum_us = 0;
        self.deltas = 0;
        self.late = 0;
        self.idle_gaps = 0;
        self.duplicate = 0;
        self.dropped = 0;
        self.phases = 0;
        self.quiet_sum_us = 0;
        self.quiet_max_us = 0;
        self.submit_sum_us = 0;
        self.commands = 0;
        self.service_count = 0;
        self.service_sum_us = 0;
        self.service_max_us = 0;
        self.pixels = 0;
        self.shared = 0;
    }

    fn report(&self) -> Option<PacingReport> {
        if self.intervals == 0 {
            return None;
        }
        // Every divisor below is its own count, and a window can hold
        // presents whose service time or phases were never recorded (a flush
        // of an offscreen resource, the first frame), so none of them is
        // assumed non-zero.
        let mean_us = self.sum_us / self.intervals;
        // Var = E[x²] − E[x]², in µs²; clamped at zero against the rounding.
        let mean_sq = self.sum_sq / u128::from(self.intervals);
        let variance = mean_sq.saturating_sub(u128::from(mean_us) * u128::from(mean_us));
        Some(PacingReport {
            intervals: self.intervals,
            mean_us,
            min_us: self.min_us,
            max_us: self.max_us,
            low_1_us: self.hist.slowest_mean_us(LOW_1_PPM),
            low_01_us: self.hist.slowest_mean_us(LOW_01_PPM),
            stddev_us: isqrt(variance),
            jitter_us: self.delta_sum_us.checked_div(self.deltas).unwrap_or(0),
            late: self.late,
            idle_gaps: self.idle_gaps,
            duplicate: self.duplicate,
            dropped: self.dropped,
            quiet_mean_us: self.quiet_sum_us.checked_div(self.phases).unwrap_or(0),
            quiet_max_us: self.quiet_max_us,
            submit_mean_us: self.submit_sum_us.checked_div(self.phases).unwrap_or(0),
            commands_mean: self.commands.checked_div(self.phases).unwrap_or(0),
            pixels_mean: self.pixels.checked_div(self.service_count).unwrap_or(0),
            service_mean_us: self
                .service_sum_us
                .checked_div(self.service_count)
                .unwrap_or(0),
            service_max_us: self.service_max_us,
            shared: self.shared,
        })
    }
}

/// Integer square root, rounded down (the standard deviation's; `u128`
/// because a window of 200 ms intervals squares past `u64`).
fn isqrt(value: u128) -> u64 {
    if value == 0 {
        return 0;
    }
    // Newton's method from an over-estimate converges from above.
    let mut x = value;
    let mut y = x.div_ceil(2);
    while y < x {
        x = y;
        y = (x + value / x) / 2;
    }
    u64::try_from(x).unwrap_or(u64::MAX)
}

/// Rolling frame-interval accumulator. One per scanout.
#[derive(Debug)]
pub struct FramePacing {
    refresh_us: u64,
    /// Presents per report ([`report_every`] of the refresh).
    report_every: u64,
    window: Stats,
    lifetime: Stats,
    /// When the previous present was answered.
    last: Option<Instant>,
    /// First control command since that present.
    first_cmd: Option<Instant>,
    /// When the current flush started being served, and how many pixels it
    /// asked the device to move.
    flush_start: Option<Instant>,
    flush_pixels: u64,
    /// Whether the current flush went through the shared path.
    flush_shared: bool,
    /// Control commands since the previous present.
    commands: u64,
    /// Frames recorded since the last report, including the ones whose
    /// interval was an idle gap — so a report always covers `REPORT_EVERY`
    /// presents even when some of them were not frames.
    since_report: u64,
}

impl Default for FramePacing {
    fn default() -> Self {
        Self::new()
    }
}

impl FramePacing {
    pub fn new() -> Self {
        Self {
            refresh_us: u64::try_from(REFRESH_PERIOD.as_micros()).unwrap_or(0),
            report_every: report_every(REFRESH_PERIOD),
            window: Stats::default(),
            lifetime: Stats::default(),
            last: None,
            first_cmd: None,
            flush_start: None,
            flush_pixels: 0,
            flush_shared: false,
            commands: 0,
            since_report: 0,
        }
    }

    /// Sets the refresh period `dropped`, `duplicate` and `late` are measured
    /// against (the guest's advertised mode), and sizes the report window to
    /// two seconds of it. A zero period disables the slot counters rather
    /// than dividing by it, and keeps the 60 Hz budget and window.
    pub fn set_refresh(&mut self, refresh: Duration) {
        self.refresh_us = u64::try_from(refresh.as_micros()).unwrap_or(0);
        self.report_every = report_every(refresh);
    }

    /// Presents per report at the current refresh.
    pub fn report_interval(&self) -> u64 {
        self.report_every
    }

    /// Notes one control-queue command. Cheap by construction: the clock is
    /// read once per frame, for the first command after a present.
    #[inline]
    pub fn note_command(&mut self) {
        if self.first_cmd.is_none() {
            self.first_cmd = Some(Instant::now());
        }
        self.commands = self.commands.saturating_add(1);
    }

    /// Marks the point where the device starts serving a flush of the scanout
    /// resource — the boundary between `submit` and `service`.
    #[inline]
    pub fn begin_flush(&mut self, pixels: u64) {
        self.flush_start = Some(Instant::now());
        self.flush_pixels = pixels;
    }

    /// Marks the current flush as served through the shared path (the
    /// display copied the frame on the GPU), counted in
    /// [`PacingReport::shared`].
    #[inline]
    pub fn note_shared(&mut self) {
        self.flush_shared = true;
    }

    /// Records a present at `now`, returning a report every
    /// [`Self::report_interval`] presents.
    pub fn record(&mut self, now: Instant) -> Option<PacingReport> {
        if std::mem::take(&mut self.flush_shared) {
            self.window.shared = self.window.shared.saturating_add(1);
            self.lifetime.shared = self.lifetime.shared.saturating_add(1);
        }
        let first_cmd = self.first_cmd.take();
        let flush_start = self.flush_start.take();
        let pixels = std::mem::take(&mut self.flush_pixels);
        let commands = std::mem::take(&mut self.commands);

        if let Some(flush_start) = flush_start {
            let us = micros(now.saturating_duration_since(flush_start));
            self.window.record_service(us, pixels);
            self.lifetime.record_service(us, pixels);
        }

        if let Some(last) = self.last {
            let delta = now.saturating_duration_since(last);
            if delta >= IDLE_GAP {
                self.window.idle_gaps = self.window.idle_gaps.saturating_add(1);
                self.lifetime.idle_gaps = self.lifetime.idle_gaps.saturating_add(1);
            } else {
                let us = micros(delta);
                self.window.record_interval(us, self.refresh_us);
                self.lifetime.record_interval(us, self.refresh_us);
                // The decomposition needs both boundaries; a frame whose
                // flush was served without ever reaching `begin_flush` (an
                // early-out flush) contributes an interval but no phases.
                if let (Some(first_cmd), Some(flush_start)) = (first_cmd, flush_start) {
                    let quiet = micros(first_cmd.saturating_duration_since(last));
                    let submit = micros(flush_start.saturating_duration_since(first_cmd));
                    self.window.record_phases(quiet, submit, commands);
                    self.lifetime.record_phases(quiet, submit, commands);
                }
            }
        }

        self.last = Some(now);
        self.since_report = self.since_report.saturating_add(1);
        if self.since_report < self.report_every {
            return None;
        }
        let report = self.window.report();
        self.since_report = 0;
        self.window.clear();
        report
    }

    /// The current window's statistics without resetting them.
    pub fn report(&self) -> Option<PacingReport> {
        self.window.report()
    }

    /// Statistics over the whole run so far.
    pub fn lifetime(&self) -> Option<PacingReport> {
        self.lifetime.report()
    }

    /// Intervals and late frames over the whole run.
    pub fn totals(&self) -> (u64, u64) {
        (self.lifetime.intervals, self.lifetime.late)
    }

    /// Writes the latest window and the whole run to `path` as JSON, so two
    /// runs can be compared with a diff.
    ///
    /// Called once per report window (two seconds of refresh slots, 120 frames
    /// at 60 Hz), which means the file is current whatever ends the VM — a
    /// clean power-off, a Ctrl+C, or a crash.
    pub fn write_json(&self, path: &Path, window: Option<&PacingReport>) -> std::io::Result<()> {
        let mut out = String::with_capacity(1024);
        {
            use std::fmt::Write;
            // A String's Write is infallible. The period the counters were
            // measured against, so runs at different rates are never compared
            // as if they were one.
            let _ = write!(out, "{{\n  \"refresh_us\": {},", self.refresh_us);
        }
        out.push_str("\n  \"window\": ");
        match window {
            Some(report) => {
                out.push('{');
                report.write_json(&mut out);
                out.push('}');
            }
            None => out.push_str("null"),
        }
        out.push_str(",\n  \"run\": ");
        match self.lifetime() {
            Some(report) => {
                out.push('{');
                report.write_json(&mut out);
                out.push('}');
            }
            None => out.push_str("null"),
        }
        out.push_str("\n}\n");
        std::fs::write(path, out)
    }
}

fn micros(d: Duration) -> u64 {
    u64::try_from(d.as_micros()).unwrap_or(u64::MAX)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn at(base: Instant, ms: u64) -> Instant {
        base + Duration::from_millis(ms)
    }

    /// A frame the way the device drives it: some commands, then the flush.
    fn frame(pacing: &mut FramePacing, first_cmd: Instant, flush: Instant, done: Instant) {
        // Two commands, the first at `first_cmd` — the clock is only read for
        // the first, so the test drives the boundary explicitly below.
        pacing.first_cmd = Some(first_cmd);
        pacing.commands = 2;
        pacing.flush_start = Some(flush);
        pacing.flush_pixels = 1920 * 1080;
        pacing.record(done);
    }

    #[test]
    fn a_steady_60hz_guest_reports_60_fps_and_no_late_frames() {
        let base = Instant::now();
        let mut pacing = FramePacing::new();
        let mut last = None;
        for index in 0..REPORT_EVERY {
            // 16.667 ms steps, in microseconds so the mean is exact.
            let now = base + Duration::from_micros(16_667 * index);
            last = pacing.record(now).or(last);
        }
        let report = last.expect("a report every REPORT_EVERY presents");
        assert_eq!(report.intervals, REPORT_EVERY - 1);
        assert_eq!(report.mean_us, 16_667);
        assert_eq!(report.late, 0);
        assert_eq!(report.fps(), 60.0);
        // One present per refresh slot: nothing repeated, nothing dropped.
        assert_eq!((report.duplicate, report.dropped), (0, 0));
        // The window was reset by the report; the run's totals are not.
        assert!(pacing.report().is_none());
        assert_eq!(pacing.totals(), (REPORT_EVERY - 1, 0));
    }

    /// The advertised refresh a profile can ask for above 60 Hz: the rate,
    /// the slot counters, the late budget and the two-second report window
    /// all follow it, and a guest locked to its refresh reads no jitter.
    #[test]
    fn steady_guests_at_120_144_and_240_hz_report_their_rate_every_two_seconds() {
        for (hz, period_us, window) in [
            (120u64, 8_333u64, 240u64),
            (144, 6_944, 288),
            (240, 4_167, 480),
        ] {
            let base = Instant::now();
            let mut pacing = FramePacing::new();
            pacing.set_refresh(Duration::from_nanos(1_000_000_000 / hz));
            assert_eq!(
                pacing.report_interval(),
                window,
                "{hz} Hz: two seconds of slots"
            );
            let mut report = None;
            for index in 0..window {
                let now = base + Duration::from_micros(period_us * index);
                if let Some(r) = pacing.record(now) {
                    assert!(report.is_none(), "{hz} Hz: one report per window");
                    report = Some(r);
                }
            }
            let report = report.expect("a report after two seconds of slots");
            assert_eq!(report.intervals, window - 1, "{hz} Hz");
            assert_eq!(report.mean_us, period_us, "{hz} Hz");
            assert_eq!(report.fps(), hz as f64, "{hz} Hz");
            assert_eq!(
                (report.duplicate, report.dropped, report.late),
                (0, 0, 0),
                "{hz} Hz: one present per slot, none late"
            );
            assert_eq!((report.stddev_us, report.jitter_us), (0, 0), "{hz} Hz");
        }
    }

    /// At 240 Hz a frame that misses one slot is late — 8.3 ms, which the
    /// old fixed 20 ms budget called on time — and a guest at half the
    /// advertised rate duplicates one slot per frame, as at 60 Hz.
    #[test]
    fn at_240_hz_the_late_budget_is_five_milliseconds() {
        let refresh = Duration::from_nanos(1_000_000_000 / 240);
        assert_eq!(late_budget_us(4_166), 4_999);
        assert_eq!(late_budget_us(16_666), 19_999, "1.2 periods at 60 Hz");
        assert_eq!(late_budget_us(0), 20_000, "no period: the 60 Hz budget");

        let base = Instant::now();
        let mut pacing = FramePacing::new();
        pacing.set_refresh(refresh);
        let mut now = base;
        pacing.record(now);
        for index in 0..100u64 {
            // 99 on time, one that missed a slot.
            now += Duration::from_micros(if index == 40 { 8_333 } else { 4_167 });
            pacing.record(now);
        }
        let report = pacing.report().expect("intervals recorded");
        assert_eq!((report.late, report.duplicate, report.dropped), (1, 1, 0));

        let mut half = FramePacing::new();
        half.set_refresh(refresh);
        for index in 0..101u64 {
            half.record(base + Duration::from_micros(8_333 * index));
        }
        let report = half.report().expect("intervals recorded");
        assert_eq!(report.fps(), 120.0);
        assert_eq!(
            (report.duplicate, report.late, report.dropped),
            (100, 100, 0)
        );
        assert_eq!(report.jitter_us, 0, "a steady half rate does not judder");
    }

    /// A present counts as dropped when it lands under half a slot after the
    /// one before: at 120 Hz, 4.2 ms apart rounds to one slot, 2 ms to none.
    #[test]
    fn presents_under_half_a_slot_apart_are_dropped() {
        let base = Instant::now();
        let mut pacing = FramePacing::new();
        pacing.set_refresh(Duration::from_nanos(1_000_000_000 / 120));
        for index in 0..101u64 {
            pacing.record(base + Duration::from_micros(4_167 * index));
        }
        let report = pacing.report().expect("intervals recorded");
        assert_eq!(report.dropped, 0, "4.2 ms rounds to one 8.3 ms slot");
        // Rounded to the nearest slot, 4.167 of 8.333 ms is one: the counter
        // calls a present early only below half a slot. Two presents 2 ms
        // apart are the unambiguous case.
        let mut burst = FramePacing::new();
        burst.set_refresh(Duration::from_nanos(1_000_000_000 / 120));
        for index in 0..101u64 {
            burst.record(base + Duration::from_micros(2_000 * index));
        }
        assert_eq!(burst.report().expect("recorded").dropped, 100);
    }

    /// Two cadences alternating (a 240 Hz guest that makes every other
    /// slot): the standard deviation is half their difference, the judder
    /// their whole difference.
    #[test]
    fn alternating_cadences_read_as_jitter() {
        let base = Instant::now();
        let mut pacing = FramePacing::new();
        pacing.set_refresh(Duration::from_nanos(1_000_000_000 / 240));
        let mut now = base;
        pacing.record(now);
        for index in 0..100u64 {
            now += Duration::from_micros(if index % 2 == 0 { 4_000 } else { 8_000 });
            pacing.record(now);
        }
        let report = pacing.report().expect("intervals recorded");
        assert_eq!(report.mean_us, 6_000);
        assert_eq!(report.stddev_us, 2_000);
        assert_eq!(report.jitter_us, 4_000);
        let mut json = String::new();
        report.write_json(&mut json);
        assert!(
            json.contains("\"stddev_us\":2000,\"jitter_us\":4000"),
            "{json}"
        );
        assert!(
            !json.contains("  "),
            "no stray whitespace in the JSON: {json}"
        );
    }

    #[test]
    fn integer_square_root_is_exact_and_rounds_down() {
        for (value, root) in [(0u128, 0u64), (1, 1), (3, 1), (4, 2), (15, 3), (16, 4)] {
            assert_eq!(isqrt(value), root, "{value}");
        }
        assert_eq!(isqrt(u128::from(u64::MAX) * u128::from(u64::MAX)), u64::MAX);
        assert_eq!(isqrt(4_000_000), 2_000);
    }

    /// The signature of the bug this module was extended for: a guest at
    /// exactly half the refresh rate leaves one empty slot per frame.
    #[test]
    fn a_30hz_guest_duplicates_one_slot_per_frame() {
        let base = Instant::now();
        let mut pacing = FramePacing::new();
        for index in 0..REPORT_EVERY {
            pacing.record(base + Duration::from_micros(33_333 * index));
        }
        let report = pacing.lifetime().expect("run statistics");
        assert_eq!(report.fps(), 30.0);
        assert_eq!(report.duplicate, REPORT_EVERY - 1);
        assert_eq!(report.dropped, 0);
        assert_eq!(report.late, REPORT_EVERY - 1);
    }

    /// Two presents inside one refresh slot: the first can never be shown.
    #[test]
    fn presents_inside_one_refresh_slot_count_as_dropped() {
        let base = Instant::now();
        let mut pacing = FramePacing::new();
        pacing.record(base);
        pacing.record(at(base, 2)); // 2 ms after: same slot
        pacing.record(at(base, 4));
        let report = pacing.report().expect("intervals recorded");
        assert_eq!(report.dropped, 2);
        assert_eq!(report.duplicate, 0);
    }

    #[test]
    fn stalls_are_counted_and_idle_gaps_are_not_frames() {
        let base = Instant::now();
        let mut pacing = FramePacing::new();
        pacing.record(base);
        pacing.record(at(base, 10)); // fine
        pacing.record(at(base, 60)); // 50 ms: late
        pacing.record(at(base, 2_000)); // 1.94 s: idle gap, not a frame
        pacing.record(at(base, 2_005)); // fine
        let report = pacing.report().expect("intervals recorded");
        assert_eq!(report.intervals, 3, "the idle gap is not an interval");
        assert_eq!(report.idle_gaps, 1);
        assert_eq!(report.late, 1);
        assert_eq!(report.min_us, 5_000);
        assert_eq!(report.max_us, 50_000);
    }

    /// The decomposition that identifies *who* is slow: quiet + submit +
    /// service must add back up to the interval.
    #[test]
    fn phases_decompose_the_interval_exactly() {
        let base = Instant::now();
        let mut pacing = FramePacing::new();
        pacing.record(base);
        // Frame: 25 ms of silence, 6 ms of submission, 2 ms of service.
        frame(&mut pacing, at(base, 25), at(base, 31), at(base, 33));
        let report = pacing.report().expect("an interval was recorded");
        assert_eq!(report.quiet_mean_us, 25_000);
        assert_eq!(report.submit_mean_us, 6_000);
        assert_eq!(report.service_mean_us, 2_000);
        assert_eq!(
            report.quiet_mean_us + report.submit_mean_us + report.service_mean_us,
            report.mean_us
        );
        assert_eq!(report.commands_mean, 2);
        assert_eq!(report.pixels_mean, 1920 * 1080);
    }

    /// Service time is averaged over the flushes in the window, and a window
    /// with no flushes reports zero rather than dividing by zero.
    #[test]
    fn service_time_is_reported_per_window() {
        let base = Instant::now();
        let mut pacing = FramePacing::new();
        pacing.record(base);
        pacing.flush_start = Some(at(base, 15));
        pacing.record(at(base, 16));
        pacing.flush_start = Some(at(base, 29));
        pacing.record(at(base, 32));
        let report = pacing.report().expect("an interval was recorded");
        assert_eq!(report.service_mean_us, 2_000);
        assert_eq!(report.service_max_us, 3_000);
        // No `note_command`, so no phase decomposition — and no divide by zero.
        assert_eq!(report.quiet_mean_us, 0);
        assert_eq!(report.submit_mean_us, 0);
    }

    /// `note_command` reads the clock once per frame, for the first command.
    #[test]
    fn only_the_first_command_of_a_frame_takes_a_timestamp() {
        let mut pacing = FramePacing::new();
        pacing.note_command();
        let first = pacing.first_cmd.expect("timestamped");
        pacing.note_command();
        pacing.note_command();
        assert_eq!(pacing.first_cmd, Some(first));
        assert_eq!(pacing.commands, 3);
        pacing.record(Instant::now());
        assert!(pacing.first_cmd.is_none(), "reset by the present");
        assert_eq!(pacing.commands, 0);
    }

    /// The tail figures average the slowest samples; with 100 intervals the
    /// 1 % low is the single worst one and the 0.1 % low is too.
    #[test]
    fn tail_figures_average_the_slowest_intervals() {
        let base = Instant::now();
        let mut pacing = FramePacing::new();
        let mut now = base;
        pacing.record(now);
        for index in 0..100u64 {
            // 99 intervals of 10 ms and one of 100 ms.
            let step = if index == 50 { 100 } else { 10 };
            now = at(now, step);
            pacing.record(now);
        }
        let report = pacing.report().expect("intervals recorded");
        assert_eq!(report.intervals, 100);
        // Bucket midpoints: 100 ms lands in the 100.0–100.1 ms bucket.
        assert_eq!(report.low_1_us, 100_050);
        assert_eq!(report.low_01_us, 100_050);
        assert_eq!(report.low_1_fps(), 10.0);
        // The mean is dominated by the 10 ms majority.
        assert!(report.mean_us < 11_500, "{}", report.mean_us);
    }

    /// Intervals past the histogram's last bucket still land in the tail.
    #[test]
    fn intervals_past_the_last_bucket_are_not_lost() {
        let mut hist = Histogram::default();
        for _ in 0..99 {
            hist.record(1_000);
        }
        hist.record(400_000);
        assert_eq!(hist.count, 100);
        assert_eq!(hist.slowest_mean_us(LOW_1_PPM), 400_000);
    }

    #[test]
    fn a_single_present_has_no_intervals_and_never_divides_by_zero() {
        let mut pacing = FramePacing::new();
        assert!(pacing.record(Instant::now()).is_none());
        assert!(pacing.report().is_none());
        assert!(pacing.lifetime().is_none());
        assert_eq!(fps_of(0), 0.0);
    }

    /// Time going backwards (a monotonic clock is monotonic, but the
    /// arithmetic must be saturating anyway) must not panic.
    #[test]
    fn out_of_order_timestamps_do_not_panic() {
        let base = Instant::now();
        let mut pacing = FramePacing::new();
        pacing.record(at(base, 100));
        pacing.first_cmd = Some(base);
        pacing.flush_start = Some(base);
        pacing.record(base);
        let report = pacing.report().expect("recorded");
        assert_eq!(report.min_us, 0);
        assert_eq!(report.quiet_mean_us, 0);
    }

    /// A refresh period of zero disables the slot counters instead of
    /// dividing by it.
    #[test]
    fn a_zero_refresh_period_disables_the_slot_counters() {
        let base = Instant::now();
        let mut pacing = FramePacing::new();
        pacing.set_refresh(Duration::ZERO);
        pacing.record(base);
        pacing.record(at(base, 100));
        let report = pacing.report().expect("recorded");
        assert_eq!((report.duplicate, report.dropped), (0, 0));
    }

    #[test]
    fn json_carries_both_the_window_and_the_run() {
        let dir = std::env::temp_dir().join(format!("entangled-pacing-{}", std::process::id()));
        std::fs::create_dir_all(&dir).expect("temp dir");
        let path = dir.join("frame-stats.json");
        let base = Instant::now();
        let mut pacing = FramePacing::new();
        pacing.record(base);
        pacing.record(at(base, 33));
        let window = pacing.report();
        pacing
            .write_json(&path, window.as_ref())
            .expect("json written");
        let text = std::fs::read_to_string(&path).expect("json readable");
        assert!(text.contains("\"window\""), "{text}");
        assert!(text.contains("\"run\""), "{text}");
        assert!(text.contains("\"refresh_us\": 16666,"), "{text}");
        assert!(text.contains("\"fps\":30.3"), "{text}");
        assert!(text.contains("\"duplicate\":1"), "{text}");
        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn shared_flushes_are_counted_per_window_and_for_the_run() {
        let base = Instant::now();
        let mut pacing = FramePacing::new();
        let mut last = None;
        for index in 0..REPORT_EVERY {
            if index % 3 != 0 {
                pacing.note_shared();
            }
            let now = base + Duration::from_micros(16_667 * index);
            last = pacing.record(now).or(last);
        }
        let report = last.expect("a report");
        assert_eq!(report.shared, REPORT_EVERY - REPORT_EVERY.div_ceil(3));
        assert_eq!(pacing.lifetime().expect("the run").shared, report.shared);
        // A present not noted is not counted, and the note does not leak into
        // the next one.
        pacing.note_shared();
        pacing.record(base + Duration::from_micros(16_667 * REPORT_EVERY));
        pacing.record(base + Duration::from_micros(16_667 * (REPORT_EVERY + 1)));
        assert_eq!(pacing.report().expect("a window").shared, 1);
        let mut json = String::new();
        report.write_json(&mut json);
        assert!(
            json.contains(&format!("\"shared\":{}", report.shared)),
            "{json}"
        );
    }
}
