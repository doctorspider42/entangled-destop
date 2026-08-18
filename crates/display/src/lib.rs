//! Host presentation (backlog EPIC 7): one `winit` window per VM, a `wgpu`
//! texture holding the guest scanout, scaling with preserved aspect ratio.
//!
//! Current state: the viewport math the renderer will use. Window and GPU
//! init land with EPIC 7 (winit/wgpu are added to this crate then).

/// Display configuration from the VM config file.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct DisplayConfig {
    pub width: u32,
    pub height: u32,
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

/// Where the guest image lands inside the host window: letterboxed, centered,
/// aspect ratio preserved (backlog MVP-705).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Viewport {
    pub x: u32,
    pub y: u32,
    pub width: u32,
    pub height: u32,
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
    }
}
