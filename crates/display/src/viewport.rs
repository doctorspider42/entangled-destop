//! Pure presentation geometry: where the guest scanout lands inside the host
//! window (backlog MVP-705). Everything here is host-side arithmetic with no
//! GPU or windowing dependency, so it is unit-tested headlessly — the windowed
//! path cannot run in CI.

use crate::DisplayError;

/// Smallest window scale factor the config accepts.
pub const MIN_SCALE: f32 = 0.1;
/// Largest window scale factor the config accepts.
pub const MAX_SCALE: f32 = 8.0;

/// Display configuration from the VM config file.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct DisplayConfig {
    /// Guest scanout width in pixels.
    pub width: u32,
    /// Guest scanout height in pixels.
    pub height: u32,
    /// Initial host window size as a multiple of the guest resolution.
    pub scale: f32,
}

impl Default for DisplayConfig {
    fn default() -> Self {
        Self {
            width: 1920,
            height: 1080,
            scale: 1.0,
        }
    }
}

impl DisplayConfig {
    /// Rejects configurations the renderer cannot honour: zero or oversized
    /// scanouts and non-finite / out-of-range scale factors.
    pub fn validate(&self) -> Result<(), DisplayError> {
        if self.width == 0
            || self.height == 0
            || u64::from(self.width) * u64::from(self.height) > crate::MAX_SCANOUT_PIXELS
        {
            return Err(DisplayError::InvalidResolution {
                width: self.width,
                height: self.height,
            });
        }
        if !self.scale.is_finite() || self.scale < MIN_SCALE || self.scale > MAX_SCALE {
            return Err(DisplayError::Config(
                "display scale must be finite and within [0.1, 8.0]",
            ));
        }
        Ok(())
    }

    /// Initial window size in logical pixels: the guest resolution scaled by
    /// [`DisplayConfig::scale`], never zero.
    pub fn initial_window_size(&self) -> (u32, u32) {
        let scale = if self.scale.is_finite() {
            self.scale.clamp(MIN_SCALE, MAX_SCALE)
        } else {
            1.0
        };
        let scaled = |v: u32| -> u32 {
            let v = (f64::from(v) * f64::from(scale)).round();
            v.clamp(1.0, f64::from(u16::MAX)) as u32
        };
        (scaled(self.width), scaled(self.height))
    }
}

/// Where the guest image lands inside the host window: letterboxed, centered,
/// aspect ratio preserved (backlog MVP-705).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Viewport {
    /// Left edge in physical window pixels.
    pub x: u32,
    /// Top edge in physical window pixels.
    pub y: u32,
    /// Width in physical window pixels.
    pub width: u32,
    /// Height in physical window pixels.
    pub height: u32,
}

impl Viewport {
    /// True when a physical window position falls inside the presented image
    /// (i.e. not on a letterbox bar).
    pub fn contains(&self, x: f64, y: f64) -> bool {
        let left = f64::from(self.x);
        let top = f64::from(self.y);
        x >= left
            && y >= top
            && x <= left + f64::from(self.width)
            && y <= top + f64::from(self.height)
    }

    /// Translates a physical window position into viewport-local coordinates.
    /// Values outside the viewport stay outside (negative or past the extent);
    /// clamping happens in [`crate::input::pointer_abs_events`], which is where
    /// the guest-visible ABS value is produced.
    pub fn to_local(&self, x: f64, y: f64) -> (f64, f64) {
        (x - f64::from(self.x), y - f64::from(self.y))
    }
}

/// Computes the letterboxed viewport for a `guest_w`×`guest_h` scanout in a
/// `win_w`×`win_h` window. Returns None while the window is zero-sized
/// (minimized), which the renderer treats as "skip presenting".
pub fn letterbox(guest_w: u32, guest_h: u32, win_w: u32, win_h: u32) -> Option<Viewport> {
    if guest_w == 0 || guest_h == 0 || win_w == 0 || win_h == 0 {
        return None;
    }
    // Compare aspect ratios via cross-multiplication to stay in integers.
    let fit_to_width =
        u64::from(win_w) * u64::from(guest_h) <= u64::from(win_h) * u64::from(guest_w);
    let (width, height) = if fit_to_width {
        let h = (u64::from(win_w) * u64::from(guest_h) / u64::from(guest_w)) as u32;
        (win_w, h.max(1))
    } else {
        let w = (u64::from(win_h) * u64::from(guest_w) / u64::from(guest_h)) as u32;
        (w.max(1), win_h)
    };
    Some(Viewport {
        x: (win_w - width) / 2,
        y: (win_h - height) / 2,
        width,
        height,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn exact_fit_fills_window() {
        let v = letterbox(1920, 1080, 1920, 1080).unwrap();
        assert_eq!(
            v,
            Viewport {
                x: 0,
                y: 0,
                width: 1920,
                height: 1080
            }
        );
    }

    #[test]
    fn wider_window_pillarboxes() {
        let v = letterbox(1920, 1080, 2560, 1080).unwrap();
        assert_eq!(v.height, 1080);
        assert_eq!(v.width, 1920);
        assert_eq!(v.x, 320);
        assert_eq!(v.y, 0);
    }

    #[test]
    fn taller_window_letterboxes() {
        let v = letterbox(1920, 1080, 1920, 1440).unwrap();
        assert_eq!(v.width, 1920);
        assert_eq!(v.height, 1080);
        assert_eq!(v.y, 180);
    }

    #[test]
    fn minimized_window_skips() {
        assert_eq!(letterbox(1920, 1080, 0, 0), None);
        assert_eq!(letterbox(1920, 1080, 800, 0), None);
        assert_eq!(letterbox(0, 0, 800, 600), None);
    }

    #[test]
    fn upscaled_viewport_keeps_aspect() {
        // 2x integer upscale of 800x600 into a 1600x1200 window.
        let v = letterbox(800, 600, 1600, 1200).unwrap();
        assert_eq!(
            v,
            Viewport {
                x: 0,
                y: 0,
                width: 1600,
                height: 1200
            }
        );
        // Non-integer scale still preserves the 4:3 ratio within a pixel.
        let v = letterbox(800, 600, 1000, 1000).unwrap();
        assert_eq!(v.width, 1000);
        assert_eq!(v.height, 750);
        assert_eq!(v.y, 125);
    }

    #[test]
    fn extremely_thin_window_keeps_one_pixel() {
        let v = letterbox(1920, 1080, 1, 1080).unwrap();
        assert_eq!(v.width, 1);
        assert_eq!(v.height, 1);
    }

    #[test]
    fn viewport_contains_and_to_local() {
        let v = Viewport {
            x: 320,
            y: 40,
            width: 1280,
            height: 720,
        };
        assert!(v.contains(320.0, 40.0));
        assert!(v.contains(1600.0, 760.0));
        assert!(!v.contains(319.0, 100.0));
        assert!(!v.contains(1000.0, 39.0));
        assert!(!v.contains(1601.0, 100.0));
        assert_eq!(v.to_local(320.0, 40.0), (0.0, 0.0));
        assert_eq!(v.to_local(300.0, 30.0), (-20.0, -10.0));
    }

    #[test]
    fn config_validation() {
        assert!(DisplayConfig::default().validate().is_ok());
        assert!(DisplayConfig {
            width: 0,
            height: 1080,
            scale: 1.0
        }
        .validate()
        .is_err());
        assert!(DisplayConfig {
            width: 100_000,
            height: 100_000,
            scale: 1.0
        }
        .validate()
        .is_err());
        assert!(DisplayConfig {
            width: 1920,
            height: 1080,
            scale: f32::NAN
        }
        .validate()
        .is_err());
        assert!(DisplayConfig {
            width: 1920,
            height: 1080,
            scale: 0.0
        }
        .validate()
        .is_err());
    }

    #[test]
    fn initial_window_size_applies_scale() {
        let cfg = DisplayConfig {
            width: 1920,
            height: 1080,
            scale: 0.5,
        };
        assert_eq!(cfg.initial_window_size(), (960, 540));
        // Bogus scales fall back to something usable instead of panicking.
        let cfg = DisplayConfig {
            width: 640,
            height: 480,
            scale: f32::INFINITY,
        };
        assert_eq!(cfg.initial_window_size(), (640, 480));
        let cfg = DisplayConfig {
            width: 640,
            height: 480,
            scale: 1e9,
        };
        let (w, h) = cfg.initial_window_size();
        assert!(w > 0 && h > 0);
    }
}
