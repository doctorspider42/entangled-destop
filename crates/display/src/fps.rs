//! The frame-rate badge: how many frames a second the guest really sends,
//! drawn in the window's top-right corner (`Ctrl+Alt+F`).
//!
//! The number is the guest's, not the window's: every flush that reached the
//! display ([`crate::sync::Waker::frames`]) over the last sample, whether or
//! not the window drew it. That is the figure a "the picture feels like
//! 15 fps" report needs. The window presenting slower than the guest is a
//! different fault, and the `display statistics` log line has it (`fps`
//! against `guest_fps`).
//!
//! Policy only, like [`crate::toolbar`]: the event loop feeds it the frame
//! count and the time, and hands its picture to the renderer.

use std::time::{Duration, Instant};

use crate::toolbar::{self, Overlay};

/// How long one sample lasts. A second reads steadily; shorter and a desktop
/// that composes on demand flickers between numbers.
pub const SAMPLE: Duration = Duration::from_secs(1);

/// `ENTANGLED_SHOW_FPS`: any value but `0`/`off` starts the window with the
/// badge showing.
pub const SHOW_FPS_ENV: &str = "ENTANGLED_SHOW_FPS";

/// Whether [`SHOW_FPS_ENV`] asks for the badge.
pub fn shown_by_env() -> bool {
    std::env::var(SHOW_FPS_ENV).is_ok_and(|v| wants(&v))
}

fn wants(value: &str) -> bool {
    !matches!(
        value.trim().to_ascii_lowercase().as_str(),
        "" | "0" | "off" | "false" | "no"
    )
}

/// The badge's state: whether it shows, the last rate, and its picture.
#[derive(Debug)]
pub struct FpsBadge {
    shown: bool,
    /// When the current sample started, and the frame count then.
    since: Option<(Instant, u64)>,
    /// Frames a second over the last whole sample; `None` until one ended.
    rate: Option<u32>,
    /// The picture, and what it was drawn for: rate, window width, DPI scale.
    cache: Option<(Option<u32>, u32, u64, Overlay)>,
    serial: u64,
}

impl FpsBadge {
    /// A badge with no sample yet, showing or hidden.
    pub fn new(shown: bool) -> Self {
        Self {
            shown,
            since: None,
            rate: None,
            cache: None,
            serial: 0,
        }
    }

    /// Whether the badge is on screen.
    pub fn shown(&self) -> bool {
        self.shown
    }

    /// Shows or hides the badge. A badge shown again starts a new sample:
    /// the frames that arrived while it was hidden are not a rate.
    pub fn toggle(&mut self) {
        self.shown = !self.shown;
        self.since = None;
        self.rate = None;
    }

    /// The frame-rate over the last whole sample.
    pub fn rate(&self) -> Option<u32> {
        self.rate
    }

    /// Feeds the guest's running frame count. Returns `true` when the number
    /// on the badge changed, which is when the window owes a redraw: an idle
    /// guest sends no frame to carry the falling number.
    pub fn sample(&mut self, frames: u64, now: Instant) -> bool {
        if !self.shown {
            return false;
        }
        let Some((since, base)) = self.since else {
            self.since = Some((now, frames));
            return false;
        };
        let elapsed = now.saturating_duration_since(since);
        if elapsed < SAMPLE {
            return false;
        }
        let rate = rate_of(frames.saturating_sub(base), elapsed);
        self.since = Some((now, frames));
        let changed = self.rate != Some(rate);
        self.rate = Some(rate);
        changed
    }

    /// When the event loop should come back for the next sample, while the
    /// badge shows.
    pub fn next_sample(&self, now: Instant) -> Option<Instant> {
        self.shown
            .then(|| self.since.map_or(now, |(since, _)| since + SAMPLE))
    }

    /// The picture for a window `window_w` physical pixels wide at DPI
    /// `scale`, or `None` while hidden. Rendered again only when the number,
    /// the width or the scale changed.
    pub fn overlay(&mut self, window_w: u32, scale: f64) -> Option<&Overlay> {
        if !self.shown {
            return None;
        }
        let scale_key = scale.to_bits();
        let fresh = matches!(
            &self.cache,
            Some((rate, w, s, _)) if *rate == self.rate && *w == window_w && *s == scale_key
        );
        if !fresh {
            self.serial += 1;
            let overlay = toolbar::render_badge(&label(self.rate), window_w, scale, self.serial);
            self.cache = Some((self.rate, window_w, scale_key, overlay));
        }
        self.cache.as_ref().map(|(_, _, _, overlay)| overlay)
    }
}

/// Frames over a duration as whole frames a second, rounded to nearest.
fn rate_of(frames: u64, elapsed: Duration) -> u32 {
    let micros = elapsed.as_micros().max(1);
    let rate = (u128::from(frames) * 1_000_000 + micros / 2) / micros;
    u32::try_from(rate).unwrap_or(u32::MAX)
}

/// The badge's text. The font has capitals and digits only.
fn label(rate: Option<u32>) -> String {
    match rate {
        Some(rate) => format!("{rate} FPS"),
        None => "- FPS".to_string(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn at(base: Instant, ms: u64) -> Instant {
        base + Duration::from_millis(ms)
    }

    #[test]
    fn a_hidden_badge_samples_nothing_and_draws_nothing() {
        let mut badge = FpsBadge::new(false);
        let t = Instant::now();
        assert!(!badge.sample(0, t));
        assert!(!badge.sample(600, at(t, 5000)));
        assert_eq!(badge.rate(), None);
        assert!(badge.overlay(1920, 1.0).is_none());
        assert_eq!(badge.next_sample(t), None);
    }

    #[test]
    fn the_rate_is_frames_over_the_sample_not_over_the_run() {
        let mut badge = FpsBadge::new(true);
        let t = Instant::now();
        assert!(
            !badge.sample(1000, t),
            "the first call only starts a sample"
        );
        assert!(!badge.sample(1010, at(t, 500)), "half a sample is no rate");
        assert!(badge.sample(1032, at(t, 1000)));
        assert_eq!(badge.rate(), Some(32));
        // A slow second: 41 frames in 1.025 s is 40 a second.
        assert!(badge.sample(1073, at(t, 2025)));
        assert_eq!(badge.rate(), Some(40));
        // The same rate again changes nothing on screen.
        assert!(!badge.sample(1113, at(t, 3025)));
    }

    #[test]
    fn an_idle_guest_falls_to_zero_and_asks_for_the_redraw() {
        let mut badge = FpsBadge::new(true);
        let t = Instant::now();
        badge.sample(0, t);
        assert!(badge.sample(60, at(t, 1000)));
        assert!(badge.sample(60, at(t, 2000)), "60 to 0 is a change");
        assert_eq!(badge.rate(), Some(0));
        assert!(!badge.sample(60, at(t, 3000)));
    }

    #[test]
    fn showing_again_forgets_the_frames_that_arrived_while_hidden() {
        let mut badge = FpsBadge::new(true);
        let t = Instant::now();
        badge.sample(0, t);
        badge.sample(60, at(t, 1000));
        badge.toggle();
        badge.toggle();
        assert_eq!(badge.rate(), None);
        assert!(!badge.sample(100_000, at(t, 60_000)));
        assert!(badge.sample(100_030, at(t, 61_000)));
        assert_eq!(badge.rate(), Some(30));
    }

    #[test]
    fn the_next_sample_is_one_sample_after_the_last() {
        let mut badge = FpsBadge::new(true);
        let t = Instant::now();
        assert_eq!(badge.next_sample(t), Some(t), "nothing sampled yet");
        badge.sample(0, t);
        assert_eq!(badge.next_sample(at(t, 300)), Some(t + SAMPLE));
    }

    #[test]
    fn the_picture_sits_in_the_top_right_corner_and_follows_the_number() {
        let mut badge = FpsBadge::new(true);
        let first = badge.overlay(1920, 1.0).expect("shown").clone();
        assert!(first.x + first.width <= 1920);
        assert!(first.x > 1920 / 2, "on the right");
        assert_eq!(
            first.pixels.len(),
            (first.width * first.height * 4) as usize
        );
        assert_eq!(
            badge.overlay(1920, 1.0).expect("shown").serial,
            first.serial
        );
        let t = Instant::now();
        badge.sample(0, t);
        badge.sample(144, at(t, 1000));
        let second = badge.overlay(1920, 1.0).expect("shown");
        assert_ne!(second.serial, first.serial);
        assert!(
            second.width > first.width,
            "\"144 FPS\" is wider than \"- FPS\""
        );
        // A window narrower than the badge still gets a picture inside it.
        let tiny = badge.overlay(40, 3.0).expect("shown");
        assert_eq!(tiny.x, 0);
    }

    #[test]
    fn the_environment_switch_takes_any_value_but_an_off_one() {
        for on in ["1", "on", "true", "yes"] {
            assert!(wants(on), "{on}");
        }
        for off in ["", "0", "off", "OFF", "false", " no "] {
            assert!(!wants(off), "{off:?}");
        }
    }

    #[test]
    fn rates_round_to_the_nearest_frame() {
        assert_eq!(rate_of(0, SAMPLE), 0);
        assert_eq!(rate_of(60, SAMPLE), 60);
        assert_eq!(rate_of(31, Duration::from_millis(1040)), 30);
        assert_eq!(rate_of(5, Duration::ZERO), 5_000_000);
    }
}
