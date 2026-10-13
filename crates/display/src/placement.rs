//! Where the window was, so the next start opens it there (2026-10-13).
//!
//! A machine's window comes back at the size, position and maximized state it
//! was closed in. The file is a few `key = value` lines beside the profile
//! (`control_api::control::window_path`), written when the window closes —
//! never while it is fullscreen or minimized, which are not places to come
//! back to — and read when it opens. A saved position that no longer lands on
//! any monitor (a laptop off its dock) is ignored rather than opening the
//! window off screen; a malformed file is ignored the same way.

use std::path::Path;

/// The window's last geometry, in physical pixels.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Placement {
    /// Left edge of the window frame.
    pub x: i32,
    /// Top edge of the window frame.
    pub y: i32,
    /// Inner width.
    pub width: u32,
    /// Inner height.
    pub height: u32,
    /// Whether it was maximized (the size is then the one it had before).
    pub maximized: bool,
}

/// How much of the window's title bar must be on a monitor to count.
const VISIBLE: i32 = 64;

impl Placement {
    /// The file's text.
    pub fn to_text(self) -> String {
        format!(
            "x = {}\ny = {}\nwidth = {}\nheight = {}\nmaximized = {}\n",
            self.x, self.y, self.width, self.height, self.maximized
        )
    }

    /// The geometry in `text`, or `None` if any field is missing or absurd.
    pub fn parse(text: &str) -> Option<Self> {
        let mut fields = std::collections::HashMap::new();
        for line in text.lines() {
            if let Some((key, value)) = line.split_once('=') {
                fields.insert(key.trim(), value.trim());
            }
        }
        let placement = Self {
            x: fields.get("x")?.parse().ok()?,
            y: fields.get("y")?.parse().ok()?,
            width: fields.get("width")?.parse().ok()?,
            height: fields.get("height")?.parse().ok()?,
            maximized: fields.get("maximized")?.parse().ok()?,
        };
        let sane = |v: u32| (crate::ux::MIN_WINDOW_HEIGHT..=16_384).contains(&v);
        (sane(placement.width) && sane(placement.height)).then_some(placement)
    }

    /// Reads `path`, or `None` if it is missing or malformed.
    pub fn load(path: &Path) -> Option<Self> {
        Self::parse(&std::fs::read_to_string(path).ok()?)
    }

    /// Writes `path`, making its directory.
    pub fn save(self, path: &Path) -> std::io::Result<()> {
        if let Some(dir) = path.parent() {
            std::fs::create_dir_all(dir)?;
        }
        std::fs::write(path, self.to_text())
    }

    /// Whether the window's title bar would be on one of `monitors` (each
    /// `(x, y, width, height)` in physical pixels).
    pub fn is_on(&self, monitors: &[(i32, i32, u32, u32)]) -> bool {
        let top = (
            self.x.saturating_add(VISIBLE),
            self.y.saturating_add(VISIBLE / 2),
        );
        monitors.iter().any(|&(mx, my, mw, mh)| {
            let right = mx.saturating_add(i32::try_from(mw).unwrap_or(i32::MAX));
            let bottom = my.saturating_add(i32::try_from(mh).unwrap_or(i32::MAX));
            (mx..right).contains(&top.0) && (my..bottom).contains(&top.1)
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const SAMPLE: Placement = Placement {
        x: -1800,
        y: 120,
        width: 1600,
        height: 900,
        maximized: false,
    };

    #[test]
    fn a_placement_round_trips_through_its_text() {
        assert_eq!(Placement::parse(&SAMPLE.to_text()), Some(SAMPLE));
        assert_eq!(Placement::parse("x = 1\ny = 2\n"), None, "fields missing");
        assert_eq!(
            Placement::parse("x = 0\ny = 0\nwidth = 5\nheight = 900\nmaximized = false\n"),
            None,
            "a width no window has"
        );
    }

    #[test]
    fn a_window_off_every_monitor_is_not_restored_there() {
        let left = (-1920, 0, 1920, 1080);
        let main = (0, 0, 2560, 1440);
        assert!(SAMPLE.is_on(&[left, main]), "on the left monitor");
        assert!(!SAMPLE.is_on(&[main]), "that monitor is gone");
        let hanging_off_top = Placement { y: -200, ..SAMPLE };
        assert!(!hanging_off_top.is_on(&[left, main]));
    }
}
