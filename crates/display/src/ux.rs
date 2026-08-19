//! Window UX policy: scaling mode, host cursor visibility, title text and the
//! initial window geometry (backlog EPIC 15, WIN-1501…1504).
//!
//! Everything here is a pure function of state the event loop already has, so
//! the whole policy is unit-tested headlessly and `host.rs` is left with nothing
//! but "ask the policy, tell winit".

use crate::{letterbox, Viewport};

/// Smallest window width the host accepts, in logical pixels (WIN-1503).
///
/// Below this the letterboxed image stops being usable; winit enforces it as the
/// window's minimum inner size, so the compositor never hands us a smaller one
/// (except zero while minimized, which presenting already skips).
pub const MIN_WINDOW_WIDTH: u32 = 640;
/// Smallest window height the host accepts, in logical pixels (WIN-1503).
pub const MIN_WINDOW_HEIGHT: u32 = 360;

/// Fraction of the monitor an initial window may cover before it is shrunk to
/// fit and opened maximized instead (WIN-1503). The remainder leaves room for
/// panels and window decorations.
const MONITOR_FIT_NUMERATOR: u64 = 9;
const MONITOR_FIT_DENOMINATOR: u64 = 10;

/// How the guest image is fitted into the window (WIN-1503/1504).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum ScaleMode {
    /// Scale to fill the window, preserving the aspect ratio: the letterboxed
    /// default. Manual resizing rescales the image smoothly.
    #[default]
    Fit,
    /// One guest pixel per host pixel, centered — no scaling, no scrollbars.
    /// A window smaller than the guest scanout falls back to [`ScaleMode::Fit`]
    /// for that frame, so the whole desktop stays visible either way.
    PixelPerfect,
}

impl ScaleMode {
    /// The other mode; what `Ctrl+Alt+O` switches to.
    #[must_use]
    pub fn toggled(self) -> Self {
        match self {
            Self::Fit => Self::PixelPerfect,
            Self::PixelPerfect => Self::Fit,
        }
    }

    /// True in 1:1 mode.
    pub fn is_pixel_perfect(self) -> bool {
        matches!(self, Self::PixelPerfect)
    }
}

/// Places a `guest_w`×`guest_h` scanout inside a `win_w`×`win_h` window
/// according to `mode`.
///
/// Returns `None` for a zero-sized (minimized) window, exactly like
/// [`letterbox`], which the renderer treats as "skip presenting".
pub fn viewport_for(
    mode: ScaleMode,
    guest_w: u32,
    guest_h: u32,
    win_w: u32,
    win_h: u32,
) -> Option<Viewport> {
    if guest_w == 0 || guest_h == 0 || win_w == 0 || win_h == 0 {
        return None;
    }
    if mode.is_pixel_perfect() && win_w >= guest_w && win_h >= guest_h {
        return Some(Viewport {
            x: (win_w - guest_w) / 2,
            y: (win_h - guest_h) / 2,
            width: guest_w,
            height: guest_h,
        });
    }
    letterbox(guest_w, guest_h, win_w, win_h)
}

/// Physical pixels from the window edge inside which the host cursor stays
/// visible even while grabbed (WIN-1501).
///
/// The cursor image is a property of the *pointer*, and once the pointer
/// crosses from the guest image onto the window's CSD frame there is no
/// reliable way to change it any more (winit defers `set_cursor` until the
/// pointer is back over the content, and on theme-less hosts — stock WSL —
/// the frame cannot set its own either). So the switch back to the visible
/// arrow must happen *before* the crossing: the outer margin of the window is
/// a "cursor visible" zone. The guest cursor sits under the host one there,
/// which is the cheaper cosmetic cost.
pub const CURSOR_EDGE_MARGIN: f64 = 16.0;

/// True when a window position is within [`CURSOR_EDGE_MARGIN`] of any window
/// edge — where the host cursor must stay visible so it survives onto the
/// decorations (see the constant's docs).
pub fn near_window_edge(x: f64, y: f64, win_w: u32, win_h: u32) -> bool {
    x < CURSOR_EDGE_MARGIN
        || y < CURSOR_EDGE_MARGIN
        || x > f64::from(win_w) - CURSOR_EDGE_MARGIN
        || y > f64::from(win_h) - CURSOR_EDGE_MARGIN
}

/// Whether the *host* cursor should be visible (WIN-1501).
///
/// The guest draws its own pointer (Weston does), so two cursors would chase
/// each other across the image. The host cursor is therefore hidden only where
/// the guest's own cursor is: inside the viewport, with the grab active.
/// Over the letterbox bars, or with the grab released, the user needs the host
/// cursor back to reach the window controls.
pub fn cursor_visible(grabbed: bool, pointer_over_guest: bool) -> bool {
    !(grabbed && pointer_over_guest)
}

/// What the title bar advertises about the window's input state (WIN-1502).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct WindowStatus {
    /// Whether host input is currently routed to the guest.
    pub grabbed: bool,
    /// Current scaling mode.
    pub mode: ScaleMode,
}

/// The grab hint appended to the window title: the user must be able to tell,
/// without touching anything, whether their keystrokes go to the guest and how
/// to get out.
pub fn grab_hint(grabbed: bool) -> &'static str {
    if grabbed {
        " — [input grabbed, Ctrl+Alt releases]"
    } else {
        " — [click to grab input]"
    }
}

/// Composes the full window title from the VM's own title and the current
/// window state.
pub fn window_title(base: &str, status: WindowStatus) -> String {
    let mut title = String::with_capacity(base.len() + 48);
    title.push_str(base);
    title.push_str(grab_hint(status.grabbed));
    if status.mode.is_pixel_perfect() {
        title.push_str(" — 1:1");
    }
    title
}

/// Initial window geometry (WIN-1503).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct InitialWindow {
    /// Logical width to request.
    pub width: u32,
    /// Logical height to request.
    pub height: u32,
    /// Whether to open the window maximized, because the requested size did not
    /// fit the monitor. The requested size is what un-maximizing restores to.
    pub maximized: bool,
}

/// Fits the configured window size to the monitor.
///
/// A 1920×1080 guest at `scale = 1.0` does not fit a 1920×1080 monitor once
/// decorations and panels are accounted for, and a window larger than the screen
/// is the single most annoying way to start a VM. So: shrink to 90% of the
/// monitor, preserving the aspect ratio, and open maximized — the user gets the
/// biggest usable image immediately, and un-maximizing gives a window that still
/// fits. `monitor` is the monitor's *logical* size, or `None` when winit cannot
/// name a monitor (some headless/remote compositors).
pub fn initial_window(req_w: u32, req_h: u32, monitor: Option<(u32, u32)>) -> InitialWindow {
    let width = req_w.max(MIN_WINDOW_WIDTH);
    let height = req_h.max(MIN_WINDOW_HEIGHT);
    let Some((mon_w, mon_h)) = monitor else {
        return InitialWindow {
            width,
            height,
            maximized: false,
        };
    };
    let available = |extent: u32| -> u32 {
        (u64::from(extent) * MONITOR_FIT_NUMERATOR / MONITOR_FIT_DENOMINATOR) as u32
    };
    let (avail_w, avail_h) = (available(mon_w), available(mon_h));
    if avail_w == 0 || avail_h == 0 || (width <= avail_w && height <= avail_h) {
        return InitialWindow {
            width,
            height,
            maximized: false,
        };
    }
    // `letterbox` is exactly "largest same-aspect box inside these bounds".
    match letterbox(width, height, avail_w, avail_h) {
        Some(fitted) => InitialWindow {
            width: fitted.width.max(MIN_WINDOW_WIDTH),
            height: fitted.height.max(MIN_WINDOW_HEIGHT),
            maximized: true,
        },
        None => InitialWindow {
            width,
            height,
            maximized: false,
        },
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn fit_mode_letterboxes_like_before() {
        let fit = viewport_for(ScaleMode::Fit, 1920, 1080, 2560, 1080).unwrap();
        assert_eq!(fit, letterbox(1920, 1080, 2560, 1080).unwrap());
    }

    #[test]
    fn pixel_perfect_centers_at_native_size() {
        let v = viewport_for(ScaleMode::PixelPerfect, 1280, 720, 1920, 1080).unwrap();
        assert_eq!(
            v,
            Viewport {
                x: 320,
                y: 180,
                width: 1280,
                height: 720
            }
        );
        // Exact fit: no bars at all.
        let v = viewport_for(ScaleMode::PixelPerfect, 1280, 720, 1280, 720).unwrap();
        assert_eq!(v.x, 0);
        assert_eq!(v.y, 0);
        assert_eq!((v.width, v.height), (1280, 720));
        // Odd remainders round the offset down, staying inside the window.
        let v = viewport_for(ScaleMode::PixelPerfect, 800, 600, 801, 601).unwrap();
        assert_eq!((v.x, v.y), (0, 0));
        assert_eq!((v.width, v.height), (800, 600));
    }

    #[test]
    fn pixel_perfect_scales_down_when_the_window_is_too_small() {
        // Narrower than the guest in one axis: fall back to fitting.
        let v = viewport_for(ScaleMode::PixelPerfect, 1920, 1080, 1280, 1080).unwrap();
        assert_eq!(v, letterbox(1920, 1080, 1280, 1080).unwrap());
        assert!(v.width <= 1280 && v.height <= 1080);
        let v = viewport_for(ScaleMode::PixelPerfect, 1920, 1080, 1920, 800).unwrap();
        assert_eq!(v, letterbox(1920, 1080, 1920, 800).unwrap());
    }

    #[test]
    fn both_modes_skip_a_minimized_window() {
        for mode in [ScaleMode::Fit, ScaleMode::PixelPerfect] {
            assert_eq!(viewport_for(mode, 1920, 1080, 0, 0), None);
            assert_eq!(viewport_for(mode, 1920, 1080, 800, 0), None);
            assert_eq!(viewport_for(mode, 0, 1080, 800, 600), None);
        }
    }

    #[test]
    fn scale_mode_toggles_both_ways() {
        assert_eq!(ScaleMode::default(), ScaleMode::Fit);
        assert_eq!(ScaleMode::Fit.toggled(), ScaleMode::PixelPerfect);
        assert_eq!(ScaleMode::PixelPerfect.toggled(), ScaleMode::Fit);
        assert!(!ScaleMode::Fit.is_pixel_perfect());
        assert!(ScaleMode::PixelPerfect.is_pixel_perfect());
    }

    #[test]
    fn the_cursor_hides_only_over_a_grabbed_guest_image() {
        assert!(!cursor_visible(true, true));
        assert!(cursor_visible(true, false), "over the letterbox bars");
        assert!(cursor_visible(false, true), "grab released");
        assert!(cursor_visible(false, false));
    }

    #[test]
    fn the_window_edge_margin_is_detected_on_all_four_sides() {
        let (w, h) = (1920, 1080);
        assert!(!near_window_edge(960.0, 540.0, w, h), "window center");
        assert!(near_window_edge(2.0, 540.0, w, h), "left");
        assert!(near_window_edge(960.0, 2.0, w, h), "top");
        assert!(near_window_edge(1918.0, 540.0, w, h), "right");
        assert!(near_window_edge(960.0, 1078.0, w, h), "bottom");
        // Just inside the margin boundary.
        assert!(near_window_edge(CURSOR_EDGE_MARGIN - 1.0, 540.0, w, h));
        assert!(!near_window_edge(CURSOR_EDGE_MARGIN + 1.0, 540.0, w, h));
    }

    #[test]
    fn the_title_states_the_grab_and_the_mode() {
        let grabbed = window_title(
            "Entangled Desktop — demo",
            WindowStatus {
                grabbed: true,
                mode: ScaleMode::Fit,
            },
        );
        assert_eq!(
            grabbed,
            "Entangled Desktop — demo — [input grabbed, Ctrl+Alt releases]"
        );
        let released = window_title(
            "vm",
            WindowStatus {
                grabbed: false,
                mode: ScaleMode::PixelPerfect,
            },
        );
        assert_eq!(released, "vm — [click to grab input] — 1:1");
        assert!(grab_hint(true).contains("Ctrl+Alt"));
        assert!(grab_hint(false).contains("click"));
    }

    #[test]
    fn an_initial_window_that_fits_is_left_alone() {
        let w = initial_window(1280, 720, Some((2560, 1440)));
        assert_eq!(
            w,
            InitialWindow {
                width: 1280,
                height: 720,
                maximized: false
            }
        );
        // No monitor information: trust the configuration.
        assert_eq!(
            initial_window(1920, 1080, None),
            InitialWindow {
                width: 1920,
                height: 1080,
                maximized: false
            }
        );
    }

    #[test]
    fn an_oversized_initial_window_shrinks_and_maximizes() {
        // The MVP default on a same-sized monitor.
        let w = initial_window(1920, 1080, Some((1920, 1080)));
        assert!(w.maximized);
        assert!(w.width <= 1728 && w.height <= 972, "{w:?}");
        // Aspect ratio preserved within a pixel.
        let ratio = f64::from(w.width) / f64::from(w.height);
        assert!((ratio - 16.0 / 9.0).abs() < 0.01, "{ratio}");
        // A tiny monitor still yields a usable, minimum-sized window.
        let w = initial_window(1920, 1080, Some((320, 240)));
        assert!(w.width >= MIN_WINDOW_WIDTH && w.height >= MIN_WINDOW_HEIGHT);
    }

    #[test]
    fn tiny_configured_sizes_are_raised_to_the_minimum() {
        let w = initial_window(320, 200, Some((2560, 1440)));
        assert_eq!(w.width, MIN_WINDOW_WIDTH);
        assert_eq!(w.height, MIN_WINDOW_HEIGHT);
        assert!(!w.maximized);
        // A zero-sized monitor cannot be reasoned about; keep the request.
        let w = initial_window(1920, 1080, Some((0, 0)));
        assert_eq!((w.width, w.height), (1920, 1080));
        assert!(!w.maximized);
    }
}
