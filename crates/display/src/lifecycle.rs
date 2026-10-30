//! Persistent lifecycle feedback. Completion is an explicit host event,
//! independent of the transient toolbar notice's timeout.
use super::*;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
/// A host operation which temporarily holds the VM's interactive session.
pub enum LifecyclePhase {
    /// Writing a durable session before the VM process exits.
    Saving,
    /// Reconstructing the saved session before its vCPUs start.
    Restoring,
}

impl LifecyclePhase {
    /// Plain-language activity text for the native window.
    pub fn title(self) -> &'static str {
        match self {
            Self::Saving => "Saving your place",
            Self::Restoring => "Waking your machine",
        }
    }
}

pub(crate) struct Loading {
    phase: Option<(LifecyclePhase, Instant)>,
    motion: bool,
    serial: u64,
    key: Option<(u64, u32, u32, u64)>,
    image: Option<Overlay>,
}

impl Loading {
    pub fn new(motion: bool) -> Self {
        Self {
            phase: None,
            motion,
            serial: 0,
            key: None,
            image: None,
        }
    }

    pub fn set(&mut self, phase: Option<LifecyclePhase>, now: Instant) {
        self.phase = phase.map(|phase| (phase, now));
        self.key = None;
        self.image = None;
    }

    pub fn phase(&self) -> Option<LifecyclePhase> {
        self.phase.map(|(phase, _)| phase)
    }

    pub fn interval(&self, focused: bool) -> Duration {
        Duration::from_millis(if self.motion && focused { 40 } else { 1000 })
    }

    fn tick(&self, now: Instant) -> u64 {
        self.phase.map_or(0, |(_, since)| {
            if self.motion {
                (now.saturating_duration_since(since).as_millis() / 40) as u64
            } else {
                0
            }
        })
    }

    pub fn needs_redraw(&self, now: Instant) -> bool {
        self.phase.is_some() && self.key.is_none_or(|key| key.0 != self.tick(now))
    }

    pub fn overlay(&mut self, window: (u32, u32), scale: f64, now: Instant) -> Option<&Overlay> {
        let (phase, _) = self.phase?;
        if window.0 == 0 || window.1 == 0 {
            return None;
        }
        let scale = if scale.is_finite() {
            scale.clamp(0.25, 3.0)
        } else {
            1.0
        };
        let key = (self.tick(now), window.0, window.1, scale.to_bits());
        if self.key != Some(key) {
            self.serial = self.serial.wrapping_add(1);
            self.image = Some(render_loading(phase, key.0, window, scale, self.serial));
            self.key = Some(key);
        }
        self.image.as_ref()
    }
}

fn render_loading(
    phase: LifecyclePhase,
    tick: u64,
    window: (u32, u32),
    dpi: f64,
    serial: u64,
) -> Overlay {
    let scale = dpi
        .min(f64::from(window.0) / 320.0)
        .min(f64::from(window.1) / 192.0);
    let width = (320.0 * scale).round().max(1.0) as u32;
    let height = (192.0 * scale).round().max(1.0) as u32;
    let mut canvas = Canvas::new(width as usize, height as usize);
    let panel = Rect {
        x: 8.0 * scale,
        y: 8.0 * scale,
        w: 304.0 * scale,
        h: 176.0 * scale,
    };
    canvas.fill(panel, PANEL, |x, y| {
        sd_round_rect(x, y, panel, 14.0 * scale)
    });
    canvas.fill(panel, STROKE, |x, y| {
        sd_round_rect(x, y, panel, 14.0 * scale).abs() - 0.5 * scale
    });
    let time = tick as f64 * 0.04;
    let direction = if phase == LifecyclePhase::Saving {
        1.0
    } else {
        -1.0
    };
    for (cx, offset) in [(144.0, 0.0), (176.0, std::f64::consts::PI)] {
        let (cx, cy, radius) = (cx * scale, 64.0 * scale, 22.0 * scale);
        let bounds = Rect {
            x: cx - radius - 3.0 * scale,
            y: cy - radius - 3.0 * scale,
            w: 2.0 * radius + 6.0 * scale,
            h: 2.0 * radius + 6.0 * scale,
        };
        let mut track = CYAN;
        track[3] = 0.22;
        canvas.fill(bounds, track, |x, y| {
            ((x - cx).hypot(y - cy) - radius).abs() - 1.2 * scale
        });
        let start = direction * time * 1.8 + offset;
        canvas.fill(bounds, CYAN, |x, y| {
            let angle = (y - cy).atan2(x - cx);
            let delta = (angle - start).rem_euclid(std::f64::consts::TAU);
            if delta < std::f64::consts::PI * 0.85 {
                ((x - cx).hypot(y - cy) - radius).abs() - 1.3 * scale
            } else {
                2.0
            }
        });
    }
    let k = font_scale(scale);
    let (text_w, _) = text_size(phase.title(), k);
    draw_text(
        &mut canvas,
        phase.title(),
        ((f64::from(width) - text_w) / 2.0).max(0.0),
        110.0 * scale,
        k,
        TEXT,
    );
    let rail = Rect {
        x: 96.0 * scale,
        y: 151.0 * scale,
        w: 128.0 * scale,
        h: 2.0 * scale,
    };
    canvas.fill(rail, STROKE, |x, y| sd_round_rect(x, y, rail, scale));
    let travel = (time * 0.55).sin() * 0.5 + 0.5;
    let light = Rect {
        x: rail.x + travel * 100.0 * scale,
        w: 28.0 * scale,
        ..rail
    };
    canvas.fill(light, CYAN, |x, y| sd_round_rect(x, y, light, scale));
    Overlay {
        serial,
        x: window.0.saturating_sub(width) / 2,
        y: window.1.saturating_sub(height) / 2,
        width,
        height,
        pixels: canvas.into_bgra(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn operation_feedback_outlives_notices_and_only_clears_on_completion() {
        let now = Instant::now();
        let mut loading = Loading::new(true);
        loading.set(Some(LifecyclePhase::Saving), now);
        assert!(loading
            .overlay((640, 480), 1.0, now + Duration::from_secs(300))
            .is_some());
        loading.set(None, now + Duration::from_secs(301));
        assert!(loading
            .overlay((640, 480), 1.0, now + Duration::from_secs(302))
            .is_none());
        assert!(!loading.needs_redraw(now + Duration::from_secs(302)));
    }

    #[test]
    fn reduced_motion_stays_static_and_pixels_remain_bounded_and_premultiplied() {
        let now = Instant::now();
        for scale in [0.01, 1.0, 2.0, 3.0, f64::NAN] {
            let mut loading = Loading::new(false);
            loading.set(Some(LifecyclePhase::Restoring), now);
            let first = loading.overlay((640, 480), scale, now).unwrap().clone();
            let later = loading
                .overlay((640, 480), scale, now + Duration::from_secs(100))
                .unwrap();
            assert_eq!(&first, later);
            assert!(later.x + later.width <= 640 && later.y + later.height <= 480);
            assert_eq!(
                later.pixels.len(),
                later.width as usize * later.height as usize * 4
            );
            assert!(later
                .pixels
                .chunks_exact(4)
                .all(|p| p[0] <= p[3] && p[1] <= p[3] && p[2] <= p[3]));
        }
    }
}
