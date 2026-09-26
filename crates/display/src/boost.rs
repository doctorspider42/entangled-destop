//! Asking the host GPU driver for its clocks while the guest draws (ADR-0004,
//! the GPU-boost amendment). Pure and unit-tested; the Vulkan call that
//! carries the request lives in `gpu_scanout` (Windows), and `host.rs` only
//! asks this module when to make it.
//!
//! # Why
//!
//! The guest's GPU work runs on the renderer's own Vulkan devices, in bursts
//! a frame apart. NVIDIA's driver reads that as a GPU that is mostly idle and
//! keeps it at P5/P8 (a few hundred MHz, memory at 810 or 405 MHz), where the
//! guest's work takes 3–10× its P0 time and every round trip waits longer.
//! `VK_NV_low_latency2`'s `vkSetLatencySleepModeNV` with `lowLatencyBoost`
//! is the one request an application may make for more: "hint to the GPU to
//! increase its power state". It is per swapchain, and only the window has
//! one, but on the RTX 2070 (580.88) it holds the **whole GPU** at P0 while it
//! is set — the renderer's devices included, and even with nothing submitted
//! at all.
//!
//! # Why gated on activity
//!
//! That last part is the cost: set and left, it holds P0 (about +12–15 W on
//! this host) through an idle desktop too. So the window asks only while the
//! guest is using the GPU — a flip reached the display, or a `SUBMIT_3D`
//! reached the device — and withdraws the request [`ACTIVE_HOLD`] after the
//! last of them. The driver's own clocks then fall about 1.8 s later.

use std::time::{Duration, Instant};

/// Environment variable that overrides the profile's `[display] gpu_boost`,
/// for the A/B measurement: `off`, `active` (the gated default) or `always`
/// (set once and left, measurement only).
pub const GPU_BOOST_ENV: &str = "ENTANGLED_GPU_BOOST";

/// How long after the guest's last GPU activity the boost stays asked for.
/// A desktop animating at 60 Hz is active every 17 ms; an idle one flips
/// nothing. A second covers the pauses inside one burst of work (vk-smoke's
/// checks, a page load) without holding an idle desktop at P0 for long.
pub const ACTIVE_HOLD: Duration = Duration::from_secs(1);

/// When the window asks the driver for its clocks.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum BoostPolicy {
    /// Never; the extension is not even enabled.
    Off,
    /// While the guest uses the GPU, and [`ACTIVE_HOLD`] after (the default
    /// of a GPU desktop with a window).
    #[default]
    WhileActive,
    /// From the first frame to the last, idle or not. For measurement.
    Always,
}

impl BoostPolicy {
    /// The policy a profile's `gpu_boost` asks for: `Some(false)` is off,
    /// anything else the gated default.
    pub fn from_profile(gpu_boost: Option<bool>) -> Self {
        match gpu_boost {
            Some(false) => Self::Off,
            Some(true) | None => Self::WhileActive,
        }
    }

    /// Parses [`GPU_BOOST_ENV`]'s value; `None` for anything else, so a typo
    /// is reported rather than silently taken.
    pub fn parse(value: &str) -> Option<Self> {
        match value.trim().to_ascii_lowercase().as_str() {
            "off" | "0" | "false" | "no" => Some(Self::Off),
            "active" | "on" | "1" | "true" | "yes" => Some(Self::WhileActive),
            "always" => Some(Self::Always),
            _ => None,
        }
    }

    /// `profile`, unless [`GPU_BOOST_ENV`] names another (a value that names
    /// nothing is warned about and ignored).
    pub fn with_env(self) -> Self {
        match std::env::var(GPU_BOOST_ENV) {
            Ok(value) => Self::parse(&value).unwrap_or_else(|| {
                tracing::warn!(
                    value,
                    "unrecognised {GPU_BOOST_ENV}, expected off, active or always; \
                     keeping the profile's"
                );
                self
            }),
            Err(_) => self,
        }
    }

    /// Whether the window's device needs the extension at all.
    pub fn enabled(self) -> bool {
        self != Self::Off
    }
}

/// The window's side of the decision: given the guest's last activity, does
/// it want the boost now, and when must it look again.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct BoostGate {
    policy: BoostPolicy,
    hold: Duration,
}

impl BoostGate {
    /// A gate for `policy` with the default [`ACTIVE_HOLD`].
    pub fn new(policy: BoostPolicy) -> Self {
        Self {
            policy,
            hold: ACTIVE_HOLD,
        }
    }

    /// The same with another hold, for tests.
    pub fn with_hold(mut self, hold: Duration) -> Self {
        self.hold = hold;
        self
    }

    /// The policy it applies.
    pub fn policy(&self) -> BoostPolicy {
        self.policy
    }

    /// Whether the boost should be asked for at `now`, the guest's last GPU
    /// activity having been at `last` (`None`: never).
    pub fn wants(&self, now: Instant, last: Option<Instant>) -> bool {
        match self.policy {
            BoostPolicy::Off => false,
            BoostPolicy::Always => true,
            BoostPolicy::WhileActive => {
                last.is_some_and(|at| now.saturating_duration_since(at) < self.hold)
            }
        }
    }

    /// When the answer of [`Self::wants`] will change by itself if nothing
    /// else happens: the end of the hold, while it runs. `None` when only new
    /// activity can change it.
    pub fn next_change(&self, now: Instant, last: Option<Instant>) -> Option<Instant> {
        match self.policy {
            BoostPolicy::WhileActive => last.map(|at| at + self.hold).filter(|&end| end > now),
            BoostPolicy::Off | BoostPolicy::Always => None,
        }
    }
}

/// What the window counts about the boost, for its statistics line.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct BoostStats {
    /// Whether it is asked for now.
    pub on: bool,
    /// Times it was asked for.
    pub raised: u64,
    /// Requests the driver refused (or that could not be made: no
    /// swapchain yet).
    pub failed: u64,
    /// Time it has been asked for, in total, in milliseconds.
    pub on_ms: u64,
}

#[cfg(test)]
mod tests {
    use super::*;

    fn at(base: Instant, ms: u64) -> Instant {
        base + Duration::from_millis(ms)
    }

    #[test]
    fn the_profile_key_maps_to_a_policy_and_absent_is_the_gated_default() {
        assert_eq!(BoostPolicy::from_profile(Some(false)), BoostPolicy::Off);
        assert_eq!(
            BoostPolicy::from_profile(Some(true)),
            BoostPolicy::WhileActive
        );
        assert_eq!(BoostPolicy::from_profile(None), BoostPolicy::WhileActive);
        assert_eq!(BoostPolicy::default(), BoostPolicy::WhileActive);
        assert!(!BoostPolicy::Off.enabled());
        assert!(BoostPolicy::WhileActive.enabled());
        assert!(BoostPolicy::Always.enabled());
    }

    #[test]
    fn the_environment_spellings_parse_and_nonsense_does_not() {
        for off in ["off", "OFF", " 0 ", "false", "no"] {
            assert_eq!(BoostPolicy::parse(off), Some(BoostPolicy::Off), "{off}");
        }
        for on in ["active", "on", "1", "true", "Yes"] {
            assert_eq!(
                BoostPolicy::parse(on),
                Some(BoostPolicy::WhileActive),
                "{on}"
            );
        }
        assert_eq!(BoostPolicy::parse("always"), Some(BoostPolicy::Always));
        for bad in ["", "max", "boost", "2"] {
            assert_eq!(BoostPolicy::parse(bad), None, "{bad:?}");
        }
    }

    #[test]
    fn off_never_wants_it_and_always_always_does() {
        let base = Instant::now();
        let off = BoostGate::new(BoostPolicy::Off);
        let always = BoostGate::new(BoostPolicy::Always);
        for last in [None, Some(base), Some(at(base, 5000))] {
            for now in [base, at(base, 10), at(base, 60_000)] {
                assert!(!off.wants(now, last));
                assert!(always.wants(now, last));
                assert_eq!(off.next_change(now, last), None);
                assert_eq!(always.next_change(now, last), None);
            }
        }
    }

    #[test]
    fn while_active_wants_it_for_the_hold_after_the_last_activity() {
        let base = Instant::now();
        let gate = BoostGate::new(BoostPolicy::WhileActive).with_hold(Duration::from_millis(1000));
        assert!(!gate.wants(base, None), "no activity yet");
        assert_eq!(gate.next_change(base, None), None);
        let last = Some(at(base, 100));
        assert!(gate.wants(at(base, 100), last));
        assert!(gate.wants(at(base, 1099), last));
        assert!(!gate.wants(at(base, 1100), last), "the hold is over");
        assert!(!gate.wants(at(base, 90_000), last));
        assert_eq!(gate.next_change(at(base, 500), last), Some(at(base, 1100)));
        assert_eq!(gate.next_change(at(base, 1100), last), None);
    }

    #[test]
    fn activity_at_sixty_hertz_keeps_it_up_without_a_gap() {
        let base = Instant::now();
        let gate = BoostGate::new(BoostPolicy::WhileActive);
        let mut last = None;
        for frame in 0..600u64 {
            let now = base + Duration::from_micros(frame * 16_667);
            last = Some(now);
            // Between two frames, the window looks whenever it wakes.
            for probe in [0, 8_000, 16_600] {
                let t = now + Duration::from_micros(probe);
                assert!(gate.wants(t, last), "frame {frame} +{probe} us");
            }
        }
        // And lets go once the desktop is idle.
        let end = last.unwrap();
        assert!(!gate.wants(end + ACTIVE_HOLD, last));
    }

    #[test]
    fn activity_timestamps_in_the_future_do_not_panic_and_count_as_now() {
        // The device thread may store a timestamp taken after the window's
        // `now`: the difference saturates.
        let base = Instant::now();
        let gate = BoostGate::new(BoostPolicy::WhileActive);
        let last = Some(at(base, 50));
        assert!(gate.wants(base, last));
        assert_eq!(
            gate.next_change(base, last),
            Some(at(base, 50) + ACTIVE_HOLD)
        );
    }
}
