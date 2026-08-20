//! Frame-pacing statistics for the scanout path (ADR-0004 phase 2's
//! measurement requirement).
//!
//! The device is the only place in the system that sees *every* frame the
//! guest presents: a compositor flush is a `RESOURCE_FLUSH` on the scanout
//! resource. Recording the interval between those flushes gives a host-side
//! frame clock for free — no guest agent, no instrumented mutter, and it
//! works the same on a 2D device, a synchronous-fence virgl device and an
//! asynchronous-fence one, which is exactly what a before/after needs.
//!
//! What the numbers mean: the interval is *guest present cadence*, not GPU
//! render time. A guest compositing at 60 Hz produces ~16.7 ms intervals; a
//! guest starved by a serialized host renderer produces long ones, and a
//! guest that pipelines produces steady ones. The `late` count (intervals
//! past [`FRAME_BUDGET`]) is the interesting figure, because a mean hides the
//! stalls that make a desktop feel broken.
//!
//! Portable, allocation-free and unit-tested; the device logs a report every
//! [`REPORT_EVERY`] frames at `info`, which is what shows up in a run's log
//! as evidence.

use std::time::{Duration, Instant};

/// Frames between reports. At 60 Hz this is one report every two seconds —
/// often enough to see a phase change (GDM → session → an app starting),
/// rare enough to keep the log readable.
pub const REPORT_EVERY: u64 = 120;

/// An interval longer than this counts as `late`. 20 ms is a 50 Hz frame: a
/// compositor targeting 60 Hz that misses this missed its deadline.
pub const FRAME_BUDGET: Duration = Duration::from_millis(20);

/// Intervals longer than this are not frame pacing at all — an idle desktop
/// that presents nothing for a second, a session switch, a guest that was
/// suspended — and would poison min/mean/late. They are counted separately.
pub const IDLE_GAP: Duration = Duration::from_millis(500);

/// One report's worth of frame-interval statistics.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct PacingReport {
    /// Intervals in this window (frames − 1 for the first window).
    pub intervals: u64,
    pub mean_us: u64,
    pub min_us: u64,
    pub max_us: u64,
    /// Intervals over [`FRAME_BUDGET`].
    pub late: u64,
    /// Gaps over [`IDLE_GAP`], excluded from the statistics above.
    pub idle_gaps: u64,
    /// Mean time the *device* spent serving a flush — the readback out of the
    /// renderer plus the push into the scanout sink — in microseconds.
    ///
    /// This is the attribution the frame interval alone cannot give: an
    /// interval of 33 ms with a 2 ms service time is a guest that paces
    /// itself, while 33 ms with a 20 ms service time is a host presentation
    /// path that cannot keep up (and therefore the thing zero-copy scanout
    /// would fix).
    pub service_mean_us: u64,
    pub service_max_us: u64,
}

impl PacingReport {
    /// Mean cadence as frames per second, rounded to one decimal — the figure
    /// a human reads first.
    pub fn fps(&self) -> f64 {
        if self.mean_us == 0 {
            return 0.0;
        }
        (1_000_000.0 / self.mean_us as f64 * 10.0).round() / 10.0
    }
}

/// Rolling frame-interval accumulator. One per scanout.
#[derive(Debug, Default)]
pub struct FramePacing {
    last: Option<Instant>,
    intervals: u64,
    sum_us: u64,
    min_us: u64,
    max_us: u64,
    late: u64,
    idle_gaps: u64,
    service_count: u64,
    service_sum_us: u64,
    service_max_us: u64,
    /// Frames recorded since the last report, including the ones whose
    /// interval was an idle gap — so a report always covers `REPORT_EVERY`
    /// presents even when some of them were not frames.
    since_report: u64,
    /// Totals over the whole VM lifetime, for a single summary line.
    total_intervals: u64,
    total_late: u64,
}

impl FramePacing {
    pub fn new() -> Self {
        Self::default()
    }

    /// Records how long the device spent serving one flush.
    pub fn record_service(&mut self, taken: Duration) {
        let us = u64::try_from(taken.as_micros()).unwrap_or(u64::MAX);
        self.service_count = self.service_count.saturating_add(1);
        self.service_sum_us = self.service_sum_us.saturating_add(us);
        self.service_max_us = self.service_max_us.max(us);
    }

    /// Records a present at `now`, returning a report every [`REPORT_EVERY`]
    /// presents.
    pub fn record(&mut self, now: Instant) -> Option<PacingReport> {
        if let Some(last) = self.last {
            let delta = now.saturating_duration_since(last);
            if delta >= IDLE_GAP {
                self.idle_gaps = self.idle_gaps.saturating_add(1);
            } else {
                let us = u64::try_from(delta.as_micros()).unwrap_or(u64::MAX);
                self.intervals = self.intervals.saturating_add(1);
                self.sum_us = self.sum_us.saturating_add(us);
                self.min_us = if self.intervals == 1 {
                    us
                } else {
                    self.min_us.min(us)
                };
                self.max_us = self.max_us.max(us);
                if delta > FRAME_BUDGET {
                    self.late = self.late.saturating_add(1);
                }
            }
        }
        self.last = Some(now);
        self.since_report = self.since_report.saturating_add(1);
        if self.since_report < REPORT_EVERY {
            return None;
        }
        let report = self.report();
        self.since_report = 0;
        self.total_intervals = self.total_intervals.saturating_add(self.intervals);
        self.total_late = self.total_late.saturating_add(self.late);
        self.intervals = 0;
        self.sum_us = 0;
        self.min_us = 0;
        self.max_us = 0;
        self.late = 0;
        self.idle_gaps = 0;
        self.service_count = 0;
        self.service_sum_us = 0;
        self.service_max_us = 0;
        report
    }

    /// The current window's statistics without resetting them.
    pub fn report(&self) -> Option<PacingReport> {
        if self.intervals == 0 {
            return None;
        }
        Some(PacingReport {
            intervals: self.intervals,
            mean_us: self.sum_us / self.intervals,
            min_us: self.min_us,
            max_us: self.max_us,
            late: self.late,
            idle_gaps: self.idle_gaps,
            // A window can hold presents whose service time was never
            // recorded (a flush of an offscreen resource), so the divisor is
            // its own count and may be zero.
            service_mean_us: self
                .service_sum_us
                .checked_div(self.service_count)
                .unwrap_or(0),
            service_max_us: self.service_max_us,
        })
    }

    /// Intervals and late frames over the whole run (reported windows only).
    pub fn totals(&self) -> (u64, u64) {
        (self.total_intervals, self.total_late)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn at(base: Instant, ms: u64) -> Instant {
        base + Duration::from_millis(ms)
    }

    #[test]
    fn a_steady_60hz_guest_reports_60_fps_and_no_late_frames() {
        let base = Instant::now();
        let mut pacing = FramePacing::new();
        let mut last = None;
        for frame in 0..REPORT_EVERY {
            // 16.667 ms steps, in microseconds so the mean is exact.
            let now = base + Duration::from_micros(16_667 * frame);
            last = pacing.record(now).or(last);
        }
        let report = last.expect("a report every REPORT_EVERY presents");
        assert_eq!(report.intervals, REPORT_EVERY - 1);
        assert_eq!(report.mean_us, 16_667);
        assert_eq!(report.late, 0);
        assert_eq!(report.fps(), 60.0);
        // The window was reset by the report.
        assert!(pacing.report().is_none());
        assert_eq!(pacing.totals(), (REPORT_EVERY - 1, 0));
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

    /// Service time is averaged over the flushes in the window, and a window
    /// with no flushes reports zero rather than dividing by zero.
    #[test]
    fn service_time_is_reported_per_window() {
        let base = Instant::now();
        let mut pacing = FramePacing::new();
        pacing.record(base);
        pacing.record_service(Duration::from_micros(1_000));
        pacing.record(at(base, 16));
        pacing.record_service(Duration::from_micros(3_000));
        let report = pacing.report().expect("an interval was recorded");
        assert_eq!(report.service_mean_us, 2_000);
        assert_eq!(report.service_max_us, 3_000);
    }

    #[test]
    fn a_single_present_has_no_intervals_and_never_divides_by_zero() {
        let mut pacing = FramePacing::new();
        assert!(pacing.record(Instant::now()).is_none());
        assert!(pacing.report().is_none());
        assert_eq!(
            PacingReport {
                intervals: 0,
                mean_us: 0,
                min_us: 0,
                max_us: 0,
                late: 0,
                idle_gaps: 0,
                service_mean_us: 0,
                service_max_us: 0,
            }
            .fps(),
            0.0
        );
    }

    /// Time going backwards (a monotonic clock is monotonic, but the
    /// arithmetic must be saturating anyway) must not panic.
    #[test]
    fn out_of_order_timestamps_do_not_panic() {
        let base = Instant::now();
        let mut pacing = FramePacing::new();
        pacing.record(at(base, 100));
        pacing.record(base);
        let report = pacing.report().expect("recorded");
        assert_eq!(report.min_us, 0);
    }
}
