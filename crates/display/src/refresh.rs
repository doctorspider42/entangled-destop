//! The window against the host monitor's refresh (ADR-0004, the high-refresh
//! amendment): which present mode the window's swapchain gets, and what a
//! guest told another rate than the monitor's looks like. Pure and
//! unit-tested; `renderer.rs` and `host.rs` only ask.
//!
//! # Why the present mode is chosen, not left to `AutoVsync`
//!
//! The window is a mirror of another clock: it draws when the guest flips,
//! not when the host monitor wants a frame. wgpu's `AutoVsync` resolves to
//! `FifoRelaxed` wherever the backend has it (Vulkan on NVIDIA does), which
//! presents a late image *at once* — torn — whenever a vblank passed with
//! nothing new, and a guest slower than the monitor makes every image late
//! (the tear shows where the compositor lets the window flip directly, as in
//! borderless fullscreen; a composed window hides it).
//!
//! Measured on the RTX 2070 at 240 Hz (ADR-0004, the high-refresh
//! amendment), the mode does not move the guest's frame rate: `Fifo`,
//! `FifoRelaxed` and `Mailbox` each gave runs between 112 and 183 guest
//! flips a second, in a bimodal host state no mode avoided. So the choice is
//! made on what each mode can do wrong:
//!
//! * `FifoRelaxed` and `Immediate` tear.
//! * `Fifo` makes the window wait for a host vblank once two images are
//!   queued — a guest faster than the monitor. That wait is not the window's
//!   alone: wgpu-core holds the device's fence lock across the swapchain
//!   acquire, and every `Queue::submit` takes it, the shared presenter's copy
//!   on the virtio-gpu worker included (read from wgpu 26's source, not
//!   measured: a guest faster than this host's monitor needs a profile that
//!   asks for more than 240 Hz, which none may).
//! * `Mailbox` never waits and never tears: the newest image wins at each
//!   vblank. It is the default; a surface without it gets plain `Fifo`.

/// Environment variable that picks the window's present mode, for the A/B
/// measurement and as an escape hatch: `mailbox`, `fifo`, `relaxed`,
/// `immediate` or `auto` (wgpu's `AutoVsync`, the old behaviour).
pub const PRESENT_MODE_ENV: &str = "ENTANGLED_PRESENT_MODE";

/// What the window asks for.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum PresentPreference {
    /// `Mailbox` where the surface has it, else `Fifo`: the default.
    #[default]
    Mailbox,
    /// Plain `Fifo`: every present waits for a host vblank.
    Fifo,
    /// `FifoRelaxed` where available, else `Fifo` — what wgpu's `AutoVsync`
    /// resolves to, and what the window used before this was chosen.
    Auto,
    /// `FifoRelaxed` where available, else `Fifo`.
    Relaxed,
    /// `Immediate` where available, else `Mailbox`, else `Fifo`: tears, for
    /// measurement only.
    Immediate,
}

impl PresentPreference {
    /// Parses [`PRESENT_MODE_ENV`]'s value; `None` for anything else, so a
    /// typo is reported rather than silently taken as the default.
    pub fn parse(value: &str) -> Option<Self> {
        match value.trim().to_ascii_lowercase().as_str() {
            "mailbox" => Some(Self::Mailbox),
            "fifo" => Some(Self::Fifo),
            "auto" | "autovsync" => Some(Self::Auto),
            "relaxed" | "fiforelaxed" | "fifo-relaxed" => Some(Self::Relaxed),
            "immediate" => Some(Self::Immediate),
            _ => None,
        }
    }

    /// The preference [`PRESENT_MODE_ENV`] names, or the default (with a
    /// warning for a value that names nothing).
    pub fn from_env() -> Self {
        match std::env::var(PRESENT_MODE_ENV) {
            Ok(value) => Self::parse(&value).unwrap_or_else(|| {
                tracing::warn!(
                    value,
                    "unrecognised {PRESENT_MODE_ENV}, expected mailbox, fifo, relaxed, \
                     immediate or auto; using mailbox"
                );
                Self::default()
            }),
            Err(_) => Self::default(),
        }
    }
}

/// The concrete mode for `preference` among what the surface offers. Always
/// a mode in `available` when `available` holds `Fifo`, which every surface
/// must (the Vulkan and DXGI specifications both require it); an empty list
/// gets `Fifo` for wgpu to refuse by name.
pub fn choose_present_mode(
    available: &[wgpu::PresentMode],
    preference: PresentPreference,
) -> wgpu::PresentMode {
    use wgpu::PresentMode::{FifoRelaxed, Immediate, Mailbox};
    let order: &[wgpu::PresentMode] = match preference {
        PresentPreference::Mailbox => &[Mailbox],
        PresentPreference::Fifo => &[],
        PresentPreference::Auto | PresentPreference::Relaxed => &[FifoRelaxed],
        PresentPreference::Immediate => &[Immediate, Mailbox],
    };
    order
        .iter()
        .copied()
        .find(|mode| available.contains(mode))
        .unwrap_or(wgpu::PresentMode::Fifo)
}

/// How a guest's advertised refresh sits against the host monitor's.
#[derive(Debug, Clone, Copy, PartialEq)]
pub enum RefreshMatch {
    /// The monitor's rate is not known (a headless host, a compositor that
    /// does not say).
    Unknown,
    /// Within 1 %: one guest frame per host refresh.
    Matched {
        /// Seconds between two frames repeated or skipped by the beat of
        /// the two clocks (infinite when they are equal).
        slip_s: f64,
    },
    /// The guest composites more frames than the monitor can show: at most
    /// `monitor` of its `guest` frames a second reach the screen, and it
    /// spends the work of the rest for nobody.
    GuestFaster {
        /// The guest's advertised rate, Hz.
        guest: f64,
        /// The monitor's rate, Hz.
        monitor: f64,
    },
    /// Each guest frame stays on screen for `repeats` host refreshes on
    /// average: motion at the guest's rate.
    GuestSlower {
        /// Host refreshes per guest frame.
        repeats: f64,
        /// Whether that is a whole number (within 1 %): an even cadence.
        /// Anything else alternates between its floor and ceiling — the
        /// judder of 144 Hz content on a 240 Hz monitor, 2, 1, 2, 2, 1 ….
        even: bool,
    },
}

/// Classifies `guest_hz` against a monitor refreshing at `monitor_mhz`
/// (winit's unit, millihertz).
pub fn refresh_match(guest_hz: u32, monitor_mhz: Option<u32>) -> RefreshMatch {
    let Some(monitor_mhz) = monitor_mhz.filter(|m| *m > 0) else {
        return RefreshMatch::Unknown;
    };
    if guest_hz == 0 {
        return RefreshMatch::Unknown;
    }
    let guest = f64::from(guest_hz);
    let monitor = f64::from(monitor_mhz) / 1000.0;
    let ratio = monitor / guest;
    if (ratio - 1.0).abs() <= 0.01 {
        let beat = (monitor - guest).abs();
        let slip_s = if beat < 1e-9 {
            f64::INFINITY
        } else {
            1.0 / beat
        };
        return RefreshMatch::Matched { slip_s };
    }
    if ratio < 1.0 {
        return RefreshMatch::GuestFaster { guest, monitor };
    }
    let even = (ratio - ratio.round()).abs() <= 0.01;
    RefreshMatch::GuestSlower {
        repeats: ratio,
        even,
    }
}

/// The sentence `entangled run` logs when the window opens: what the user
/// sees with this guest refresh on this monitor. `None` when it is fine to
/// say nothing (the monitor is unknown).
pub fn refresh_sentence(guest_hz: u32, monitor_mhz: Option<u32>) -> Option<(bool, String)> {
    match refresh_match(guest_hz, monitor_mhz) {
        RefreshMatch::Unknown => None,
        RefreshMatch::Matched { slip_s } => {
            Some((
                false,
                if slip_s.is_finite() {
                    format!(
                        "the guest's {guest_hz} Hz matches the monitor: one guest frame per \
                     refresh, one repeated or skipped every {slip_s:.0} s"
                    )
                } else {
                    format!("the guest's {guest_hz} Hz matches the monitor: one guest frame per refresh")
                },
            ))
        }
        RefreshMatch::GuestFaster { guest, monitor } => Some((
            true,
            format!(
                "the guest is told {guest:.0} Hz and this monitor refreshes at {monitor:.1} Hz: \
                 at most {monitor:.0} of its frames a second reach the screen, and the guest \
                 composites the other {:.0} for nobody. Set [display] refresh_hz = {} in the \
                 profile",
                guest - monitor,
                monitor.round()
            ),
        )),
        RefreshMatch::GuestSlower { repeats, even } => Some((
            false,
            if even {
                format!(
                    "the guest's {guest_hz} Hz on a {:.1} Hz monitor: each guest frame shows \
                     for {repeats:.0} refreshes, an even cadence",
                    f64::from(monitor_mhz.unwrap_or(0)) / 1000.0
                )
            } else {
                format!(
                    "the guest's {guest_hz} Hz on a {:.1} Hz monitor: each guest frame shows \
                     for {} or {} refreshes ({repeats:.2} on average), so motion judders a \
                     little; a profile refresh_hz dividing the monitor's rate evenly avoids it",
                    f64::from(monitor_mhz.unwrap_or(0)) / 1000.0,
                    repeats.floor(),
                    repeats.ceil()
                )
            },
        )),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use wgpu::PresentMode::{AutoVsync, Fifo, FifoRelaxed, Immediate, Mailbox};

    #[test]
    fn the_default_is_mailbox_and_never_a_mode_that_tears() {
        let nvidia_vulkan = [Fifo, FifoRelaxed, Immediate, Mailbox];
        let dx12 = [Mailbox, Fifo, Immediate];
        let fifo_only = [Fifo];
        assert_eq!(
            choose_present_mode(&nvidia_vulkan, PresentPreference::default()),
            Mailbox
        );
        assert_eq!(
            choose_present_mode(&dx12, PresentPreference::default()),
            Mailbox
        );
        assert_eq!(
            choose_present_mode(&fifo_only, PresentPreference::default()),
            Fifo
        );
        // A surface with relaxed and immediate but no mailbox gets plain Fifo.
        assert_eq!(
            choose_present_mode(
                &[Fifo, FifoRelaxed, Immediate],
                PresentPreference::default()
            ),
            Fifo
        );
    }

    #[test]
    fn auto_is_what_wgpus_autovsync_resolves_to() {
        assert_eq!(
            choose_present_mode(&[Fifo, FifoRelaxed, Mailbox], PresentPreference::Auto),
            FifoRelaxed
        );
        assert_eq!(
            choose_present_mode(&[Fifo, Mailbox], PresentPreference::Auto),
            Fifo
        );
        assert_eq!(
            choose_present_mode(&[Fifo, Mailbox], PresentPreference::Fifo),
            Fifo
        );
        assert_eq!(
            choose_present_mode(&[Fifo, Mailbox], PresentPreference::Immediate),
            Mailbox
        );
        // Never an automatic mode: the answer is always concrete.
        for preference in [
            PresentPreference::Mailbox,
            PresentPreference::Fifo,
            PresentPreference::Auto,
            PresentPreference::Relaxed,
            PresentPreference::Immediate,
        ] {
            assert_ne!(choose_present_mode(&[], preference), AutoVsync);
            assert_eq!(choose_present_mode(&[], preference), Fifo);
        }
    }

    #[test]
    fn the_environment_spellings() {
        assert_eq!(
            PresentPreference::parse("mailbox"),
            Some(PresentPreference::Mailbox)
        );
        assert_eq!(
            PresentPreference::parse(" FIFO "),
            Some(PresentPreference::Fifo)
        );
        assert_eq!(
            PresentPreference::parse("auto"),
            Some(PresentPreference::Auto)
        );
        assert_eq!(
            PresentPreference::parse("fifo-relaxed"),
            Some(PresentPreference::Relaxed)
        );
        assert_eq!(
            PresentPreference::parse("immediate"),
            Some(PresentPreference::Immediate)
        );
        assert_eq!(PresentPreference::parse("vsync"), None);
    }

    #[test]
    fn a_guest_at_the_monitors_rate_slips_rarely() {
        // 240 advertised on a 239.76 Hz panel: one slip every ~4 s.
        match refresh_match(240, Some(239_760)) {
            RefreshMatch::Matched { slip_s } => assert!((slip_s - 4.17).abs() < 0.01, "{slip_s}"),
            other => panic!("{other:?}"),
        }
        assert_eq!(
            refresh_match(60, Some(60_000)),
            RefreshMatch::Matched {
                slip_s: f64::INFINITY
            }
        );
        let (warn, text) = refresh_sentence(240, Some(239_760)).expect("a sentence");
        assert!(!warn && text.contains("every 4 s"), "{text}");
    }

    #[test]
    fn a_guest_faster_than_the_monitor_is_a_warning_naming_the_fix() {
        assert_eq!(
            refresh_match(240, Some(60_000)),
            RefreshMatch::GuestFaster {
                guest: 240.0,
                monitor: 60.0
            }
        );
        let (warn, text) = refresh_sentence(240, Some(59_940)).expect("a sentence");
        assert!(warn, "{text}");
        assert!(text.contains("refresh_hz = 60"), "{text}");
        assert!(text.contains("180 for nobody"), "{text}");
    }

    #[test]
    fn a_slower_guest_is_even_only_on_a_divisor() {
        assert_eq!(
            refresh_match(120, Some(240_000)),
            RefreshMatch::GuestSlower {
                repeats: 2.0,
                even: true
            }
        );
        match refresh_match(60, Some(239_760)) {
            RefreshMatch::GuestSlower { repeats, even } => {
                assert!(even, "3.996 is four refreshes a frame, within 1 %");
                assert!((repeats - 3.996).abs() < 1e-9, "{repeats}");
            }
            other => panic!("{other:?}"),
        }
        match refresh_match(144, Some(240_000)) {
            RefreshMatch::GuestSlower { repeats, even } => {
                assert!(!even);
                assert!((repeats - 1.6667).abs() < 0.001, "{repeats}");
            }
            other => panic!("{other:?}"),
        }
        let (warn, text) = refresh_sentence(144, Some(240_000)).expect("a sentence");
        assert!(!warn && text.contains("1 or 2 refreshes"), "{text}");
    }

    #[test]
    fn nothing_is_said_without_a_monitor() {
        assert_eq!(refresh_match(60, None), RefreshMatch::Unknown);
        assert_eq!(refresh_match(60, Some(0)), RefreshMatch::Unknown);
        assert_eq!(refresh_match(0, Some(60_000)), RefreshMatch::Unknown);
        assert!(refresh_sentence(60, None).is_none());
    }
}
