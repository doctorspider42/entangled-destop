//! The refresh rate a new machine's virtual monitor is given (ADR-0004, the
//! high-refresh amendment): the host's primary monitor's, rounded to a rate
//! monitors actually run at — and, where that is faster than the GPU desktop
//! was measured to keep up with, the largest whole fraction of it that the
//! desktop does, so every guest frame stays on screen for the same number of
//! host refreshes.
//!
//! The rule is pure ([`default_refresh_hz`]) and tested with injected
//! monitor rates; the query ([`host_monitor`], behind the `host-display`
//! feature) is per host:
//!
//! * **Windows**: `EnumDisplaySettingsW(NULL, ENUM_CURRENT_SETTINGS)`, the
//!   primary display's current mode. Windows reports whole hertz and rounds
//!   down, so a 239.76 Hz panel says 239 — which the rounding here turns back
//!   into 240.
//! * **Linux**: the preferred detailed timing of the first connected DRM
//!   connector's EDID (`/sys/class/drm/card*-*/edid`), in the connectors'
//!   name order. There is no "primary" at the DRM level; the preferred mode is
//!   what the panel runs at unless someone chose otherwise. WSL has no DRM
//!   connectors at all, so it answers `None` — and the manager, which runs on
//!   Windows and drives a WSL engine, writes the Windows answer itself.
//!
//! Either way `None` means 60 Hz, what every machine got before.

/// Rates monitors are sold at. A reported rate within [`SNAP_TOLERANCE`] of
/// one of these is taken as that one: 59.94 is 60, 119.88 is 120, 143.9 is
/// 144, and Windows' 239 (a 239.76 Hz mode, rounded down) is 240.
pub const STANDARD_REFRESH_RATES: [u32; 12] =
    [60, 72, 75, 90, 100, 120, 144, 165, 170, 180, 200, 240];

/// How close a reported rate must be to a standard one to be snapped to it,
/// as a fraction of the standard rate (1.5 %).
pub const SNAP_TOLERANCE: f64 = 0.015;

/// Fewest hertz a derived default gives a guest: what a machine got before
/// the default was derived. A monitor slower than this (a 50 Hz television)
/// still gets 60 — the guest paces itself to the EDID, and 60 is the rate
/// every guest desktop is tuned for.
pub const MIN_DEFAULT_REFRESH_HZ: u32 = 60;

/// The fastest refresh the GPU desktop was measured to keep up with
/// (ADR-0004, the high-refresh amendment; RTX 2070, GNOME 50 on Zink over
/// Venus, windowed and headless): at 144 Hz GNOME filled 96–99 % of the
/// slots with vsync'd clients (138–143 flips a second, interval standard
/// deviation 0.6–1.3 ms); at 240 Hz it filled 47–76 % (112–183), missing
/// slots at random — a deviation of 2.0–2.8 ms, half a period, which is
/// judder the user sees.
pub const SUSTAINED_REFRESH_HZ: u32 = 144;

// A derived default is always a rate a profile may hold.
const _: () = assert!(
    MIN_DEFAULT_REFRESH_HZ >= crate::MIN_REFRESH_HZ
        && MAX_DEFAULT_REFRESH_HZ <= crate::MAX_REFRESH_HZ
);

/// A reported rate past this is taken as this: no monitor refreshes faster,
/// and it bounds the search for a whole fraction.
const MAX_MONITOR_HZ: u32 = 1_000;

/// Most hertz a derived default gives a guest: [`SUSTAINED_REFRESH_HZ`]. A
/// profile may still ask for up to [`crate::MAX_REFRESH_HZ`].
pub const MAX_DEFAULT_REFRESH_HZ: u32 = SUSTAINED_REFRESH_HZ;

/// The `[display] refresh_hz` a new machine gets when nobody chose one, from
/// the host monitor's rate in hertz (`None` when it could not be read).
///
/// 1. Snapped to the nearest [`STANDARD_REFRESH_RATES`] entry within
///    [`SNAP_TOLERANCE`], otherwise rounded to whole hertz (the 239 Windows
///    reports for a 239.76 Hz mode is 240).
/// 2. At or below [`SUSTAINED_REFRESH_HZ`]: that rate — one guest frame per
///    host refresh.
/// 3. Above it: the largest whole fraction of it, `rate / k` for a whole `k`
///    and a whole number of hertz, that is at most [`SUSTAINED_REFRESH_HZ`]
///    and at least [`MIN_DEFAULT_REFRESH_HZ`] — 240 is 120, 360 is 120, 180
///    is 90 — so each guest frame shows for exactly `k` host refreshes. A
///    rate with no such fraction (165) gets [`SUSTAINED_REFRESH_HZ`].
/// 4. Never below [`MIN_DEFAULT_REFRESH_HZ`]; `None`, or a rate that is not a
///    finite positive number, is 60.
///
/// Why not simply the monitor's rate: a guest told more than it can deliver
/// misses slots at random, and a guest told a rate that does not divide the
/// monitor's shows its frames for an uneven number of host refreshes (144 on
/// 240 alternates 2, 2, 1). Both judder; a whole fraction of the monitor the
/// guest keeps up with does not (ADR-0004, the high-refresh amendment, has
/// the measurements). A profile that says 240 still gets 240.
pub fn default_refresh_hz(monitor_hz: Option<f64>) -> u32 {
    let Some(hz) = monitor_hz.filter(|hz| hz.is_finite() && *hz > 0.0) else {
        return MIN_DEFAULT_REFRESH_HZ;
    };
    let snapped = STANDARD_REFRESH_RATES
        .iter()
        .copied()
        .map(|rate| (rate, (hz - f64::from(rate)).abs() / f64::from(rate)))
        .filter(|(_, off)| *off <= SNAP_TOLERANCE)
        .min_by(|a, b| a.1.total_cmp(&b.1))
        .map(|(rate, _)| rate);
    let rounded = snapped.unwrap_or_else(|| {
        // `hz` is finite and positive; nothing refreshes faster than
        // MAX_MONITOR_HZ, which also bounds the search for a fraction below.
        let whole = hz.round();
        if whole >= f64::from(MAX_MONITOR_HZ) {
            MAX_MONITOR_HZ
        } else {
            whole as u32
        }
    });
    if rounded <= SUSTAINED_REFRESH_HZ {
        return rounded.max(MIN_DEFAULT_REFRESH_HZ);
    }
    (2..=rounded / MIN_DEFAULT_REFRESH_HZ)
        .filter(|k| rounded % k == 0)
        .map(|k| rounded / k)
        .find(|fraction| (MIN_DEFAULT_REFRESH_HZ..=SUSTAINED_REFRESH_HZ).contains(fraction))
        .unwrap_or(SUSTAINED_REFRESH_HZ)
}

/// What the host said about its monitor, and where it came from.
#[derive(Debug, Clone, PartialEq)]
pub struct HostMonitor {
    /// The monitor's refresh rate, in hertz.
    pub refresh_hz: f64,
    /// Where it was read: "the primary display (EnumDisplaySettings)", or
    /// "card0-DP-1's EDID".
    pub source: String,
}

/// The refresh rate, in hertz, of an EDID base block's preferred (first)
/// detailed timing: its pixel clock over its total pixels. `None` for a
/// block that is too short, has a bad header or checksum, or whose first
/// descriptor is not a timing.
pub fn edid_preferred_refresh_hz(edid: &[u8]) -> Option<f64> {
    let block = edid.get(..128)?;
    if block[..8] != [0x00, 0xFF, 0xFF, 0xFF, 0xFF, 0xFF, 0xFF, 0x00] {
        return None;
    }
    if block.iter().fold(0u8, |sum, b| sum.wrapping_add(*b)) != 0 {
        return None;
    }
    let d = &block[54..72];
    let clock_10khz = u32::from(d[0]) | (u32::from(d[1]) << 8);
    if clock_10khz == 0 {
        // A display descriptor, not a timing.
        return None;
    }
    let h_active = u32::from(d[2]) | ((u32::from(d[4]) >> 4) << 8);
    let h_blank = u32::from(d[3]) | ((u32::from(d[4]) & 0x0F) << 8);
    let v_active = u32::from(d[5]) | ((u32::from(d[7]) >> 4) << 8);
    let v_blank = u32::from(d[6]) | ((u32::from(d[7]) & 0x0F) << 8);
    let total = u64::from(h_active + h_blank) * u64::from(v_active + v_blank);
    if total == 0 {
        return None;
    }
    Some(f64::from(clock_10khz) * 10_000.0 / total as f64)
}

/// The host's primary monitor, or `None` when there is none to read (a
/// headless host, WSL, a remote session that does not say).
#[cfg(feature = "host-display")]
pub fn host_monitor() -> Option<HostMonitor> {
    imp::host_monitor()
}

/// [`default_refresh_hz`] of [`host_monitor`], with the monitor it came from.
#[cfg(feature = "host-display")]
pub fn host_default_refresh_hz() -> (u32, Option<HostMonitor>) {
    let monitor = host_monitor();
    (
        default_refresh_hz(monitor.as_ref().map(|m| m.refresh_hz)),
        monitor,
    )
}

#[cfg(all(feature = "host-display", windows))]
mod imp {
    use super::HostMonitor;
    use windows::Win32::Graphics::Gdi::{EnumDisplaySettingsW, DEVMODEW, ENUM_CURRENT_SETTINGS};

    pub(super) fn host_monitor() -> Option<HostMonitor> {
        let mut mode = DEVMODEW {
            dmSize: u16::try_from(std::mem::size_of::<DEVMODEW>()).ok()?,
            ..Default::default()
        };
        // SAFETY: a null device name means the display this thread's desktop
        // is on (the primary); `mode` is a live, zeroed DEVMODEW whose dmSize
        // says how much the call may write, and it outlives the call.
        let ok = unsafe {
            EnumDisplaySettingsW(
                windows::core::PCWSTR::null(),
                ENUM_CURRENT_SETTINGS,
                &mut mode,
            )
        };
        // 0 and 1 are "the hardware's default", which names no rate.
        if !ok.as_bool() || mode.dmDisplayFrequency <= 1 {
            return None;
        }
        Some(HostMonitor {
            refresh_hz: f64::from(mode.dmDisplayFrequency),
            source: "the primary display (EnumDisplaySettings)".into(),
        })
    }
}

#[cfg(all(feature = "host-display", target_os = "linux"))]
mod imp {
    use super::{edid_preferred_refresh_hz, HostMonitor};

    pub(super) fn host_monitor() -> Option<HostMonitor> {
        let mut connectors: Vec<std::path::PathBuf> = std::fs::read_dir("/sys/class/drm")
            .ok()?
            .filter_map(Result::ok)
            .map(|entry| entry.path())
            .filter(|path| {
                path.file_name()
                    .and_then(|name| name.to_str())
                    .is_some_and(|name| name.starts_with("card") && name.contains('-'))
            })
            .collect();
        connectors.sort();
        connectors.into_iter().find_map(|connector| {
            let status = std::fs::read_to_string(connector.join("status")).ok()?;
            if status.trim() != "connected" {
                return None;
            }
            let edid = std::fs::read(connector.join("edid")).ok()?;
            let refresh_hz = edid_preferred_refresh_hz(&edid)?;
            let name = connector.file_name()?.to_string_lossy().into_owned();
            Some(HostMonitor {
                refresh_hz,
                source: format!("{name}'s EDID"),
            })
        })
    }
}

#[cfg(all(feature = "host-display", not(any(windows, target_os = "linux"))))]
mod imp {
    use super::HostMonitor;

    pub(super) fn host_monitor() -> Option<HostMonitor> {
        None
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_host_monitor_rate_becomes_the_standard_rate_it_is() {
        for (monitor, guest) in [
            (60.0, 60),
            (59.94, 60),
            (59.0, 60),
            (75.0, 75),
            (100.0, 100),
            (119.88, 120),
            (120.0, 120),
            (143.0, 144),
            (143.98, 144),
            (144.0, 144),
        ] {
            assert_eq!(default_refresh_hz(Some(monitor)), guest, "{monitor} Hz");
        }
    }

    /// Above the rate the desktop keeps up with, the largest whole fraction
    /// of the monitor's: this project's 239.76 Hz panel (239 to Windows) gets
    /// 120, two host refreshes a frame.
    #[test]
    fn a_monitor_faster_than_the_desktop_keeps_up_with_gets_a_whole_fraction() {
        for (monitor, guest) in [
            (239.0, 120),
            (239.76, 120),
            (240.0, 120),
            (180.0, 90),
            (200.0, 100),
            (170.0, 85),
            (288.0, 144),
            (300.0, 100),
            (360.0, 120),
            (480.0, 120),
            (500.0, 125),
            // No whole fraction between 60 and 144: the sustained rate.
            (164.8, 144),
            (165.0, 144),
            // 155.6 rounds to 156, and half of that is whole.
            (155.6, 78),
        ] {
            assert_eq!(default_refresh_hz(Some(monitor)), guest, "{monitor} Hz");
        }
        // Every whole rate above the sustained one gets the sustained rate or
        // an exact fraction of itself (no standard rate is within 1.5 % of a
        // rate this far from them, except the ones snapped above).
        for hz in [150u32, 175, 190, 210, 250, 280, 320, 400, 540, 600, 1000] {
            let guest = default_refresh_hz(Some(f64::from(hz)));
            assert!(
                guest == SUSTAINED_REFRESH_HZ || hz % guest == 0,
                "{hz} Hz gave {guest}"
            );
            assert!((MIN_DEFAULT_REFRESH_HZ..=SUSTAINED_REFRESH_HZ).contains(&guest));
        }
    }

    #[test]
    fn a_rate_no_monitor_is_sold_at_is_rounded_to_whole_hertz() {
        assert_eq!(default_refresh_hz(Some(85.0)), 85);
        assert_eq!(default_refresh_hz(Some(110.4)), 110);
        assert_eq!(default_refresh_hz(Some(137.4)), 137);
    }

    #[test]
    fn the_default_is_kept_within_60_through_144() {
        assert_eq!(default_refresh_hz(Some(50.0)), MIN_DEFAULT_REFRESH_HZ);
        assert_eq!(default_refresh_hz(Some(30.0)), MIN_DEFAULT_REFRESH_HZ);
        assert_eq!(default_refresh_hz(Some(1.0)), MIN_DEFAULT_REFRESH_HZ);
        assert!(default_refresh_hz(Some(1e300)) <= MAX_DEFAULT_REFRESH_HZ);
        assert_eq!(MAX_DEFAULT_REFRESH_HZ, SUSTAINED_REFRESH_HZ);
        // Every default is a rate a profile may hold.
        for hz in 0..600 {
            let guest = default_refresh_hz(Some(f64::from(hz)));
            assert!((crate::MIN_REFRESH_HZ..=crate::MAX_REFRESH_HZ).contains(&guest));
        }
    }

    #[test]
    fn no_monitor_or_nonsense_is_60() {
        assert_eq!(default_refresh_hz(None), 60);
        assert_eq!(default_refresh_hz(Some(0.0)), 60);
        assert_eq!(default_refresh_hz(Some(-144.0)), 60);
        assert_eq!(default_refresh_hz(Some(f64::NAN)), 60);
        assert_eq!(default_refresh_hz(Some(f64::INFINITY)), 60);
    }

    /// A block in the shape `virtio_gpu::edid` and real monitors write:
    /// the preferred timing in descriptor 1.
    fn edid(width: u32, height: u32, h_blank: u32, v_blank: u32, clock_10khz: u32) -> Vec<u8> {
        let mut e = vec![0u8; 128];
        e[..8].copy_from_slice(&[0x00, 0xFF, 0xFF, 0xFF, 0xFF, 0xFF, 0xFF, 0x00]);
        let d = &mut e[54..72];
        d[0] = clock_10khz as u8;
        d[1] = (clock_10khz >> 8) as u8;
        d[2] = width as u8;
        d[3] = h_blank as u8;
        d[4] = (((width >> 8) as u8) << 4) | ((h_blank >> 8) as u8 & 0x0F);
        d[5] = height as u8;
        d[6] = v_blank as u8;
        d[7] = (((height >> 8) as u8) << 4) | ((v_blank >> 8) as u8 & 0x0F);
        let sum = e[..127].iter().fold(0u8, |acc, b| acc.wrapping_add(*b));
        e[127] = sum.wrapping_neg();
        e
    }

    #[test]
    fn an_edids_preferred_timing_gives_its_refresh() {
        // 1920x1080 CVT-RB at 60 Hz: 2080 x 1125 x 60 = 140.40 MHz.
        let hz = edid_preferred_refresh_hz(&edid(1920, 1080, 160, 45, 14_040)).expect("a timing");
        assert!((hz - 60.0).abs() < 1e-9, "{hz}");
        // A 240 Hz gaming panel's own block (2080 x 1144, 571.13 MHz ~ 240).
        let hz = edid_preferred_refresh_hz(&edid(1920, 1080, 160, 64, 57_113)).expect("a timing");
        assert!((hz - 240.0).abs() < 0.05, "{hz}");
        assert_eq!(default_refresh_hz(Some(hz)), 120, "a whole fraction of 240");
        // With an extension block behind it, only the base block counts.
        let mut two = edid(1920, 1080, 160, 45, 14_040);
        two.extend_from_slice(&[0xAB; 128]);
        assert!(edid_preferred_refresh_hz(&two).is_some());
    }

    #[test]
    fn a_broken_edid_names_no_rate() {
        let good = edid(1920, 1080, 160, 45, 14_040);
        assert!(edid_preferred_refresh_hz(&good[..127]).is_none(), "short");
        let mut header = good.clone();
        header[1] = 0;
        assert!(edid_preferred_refresh_hz(&header).is_none(), "header");
        let mut sum = good.clone();
        sum[60] ^= 1;
        assert!(edid_preferred_refresh_hz(&sum).is_none(), "checksum");
        let not_a_timing = edid(0, 0, 0, 0, 0);
        assert!(
            edid_preferred_refresh_hz(&not_a_timing).is_none(),
            "descriptor"
        );
        assert!(edid_preferred_refresh_hz(&[]).is_none());
    }

    #[cfg(feature = "host-display")]
    #[test]
    fn this_hosts_default_is_a_rate_a_profile_may_hold() {
        let (hz, monitor) = host_default_refresh_hz();
        assert!((MIN_DEFAULT_REFRESH_HZ..=MAX_DEFAULT_REFRESH_HZ).contains(&hz));
        if let Some(monitor) = monitor {
            assert!(monitor.refresh_hz > 0.0, "{monitor:?}");
            assert_eq!(hz, default_refresh_hz(Some(monitor.refresh_hz)));
        }
    }
}
