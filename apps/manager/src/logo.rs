//! Vector artwork drawn with the egui painter (GUI-1606): the Entangled mark —
//! two tilted orbit ellipses with a pair of entangled particles running along
//! them — and the small "entangled particles" spinner used as the Installing
//! status indicator.
//!
//! Everything here is procedural: no image assets, no fonts, resolution-free.

use egui::{Color32, Painter, Pos2, Rect, Stroke, Vec2};

use crate::theme;

/// Points per ring; 72 keeps the ellipse smooth at logo sizes.
const RING_STEPS: usize = 72;
/// Ring tilt, radians. The two rings mirror each other.
const RING_TILT: f32 = 0.52;

/// Draws the mark centred in `rect`, animated by `time` (seconds).
///
/// `alpha` scales the whole drawing so callers can fade it in.
pub fn paint_mark(painter: &Painter, rect: Rect, time: f64, alpha: f32) {
    let center = rect.center();
    let radius = rect.width().min(rect.height()) * 0.5;
    let rx = radius * 0.94;
    let ry = radius * 0.40;
    let phase = (time * 0.9) as f32;

    for (index, tilt) in [RING_TILT, -RING_TILT].into_iter().enumerate() {
        // Cyan ring one way, violet ring the other, each with a gradient along
        // its own path so the two feel like one continuous system.
        let (from, to) = if index == 0 {
            (theme::CYAN, theme::VIOLET_DEEP)
        } else {
            (theme::VIOLET, theme::CYAN_DEEP)
        };
        paint_ring(
            painter,
            center,
            rx,
            ry,
            tilt,
            from,
            to,
            alpha,
            radius * 0.075,
        );
    }

    // The entangled pair: antipodal on their rings, so they always mirror.
    let a = ring_point(center, rx, ry, RING_TILT, phase);
    let b = ring_point(center, rx, ry, -RING_TILT, phase + std::f32::consts::PI);
    paint_link(painter, a, b, alpha, radius * 0.05);
    paint_particle(painter, a, radius * 0.115, theme::CYAN, alpha);
    paint_particle(painter, b, radius * 0.115, theme::VIOLET, alpha);

    // Faint nucleus glow where the rings cross.
    for (r, fade) in [(radius * 0.30, 0.05), (radius * 0.16, 0.10)] {
        painter.circle_filled(center, r, fade_white(fade * alpha));
    }
}

/// The Installing indicator: two dots orbiting a shared centre, joined by a
/// fading line. Sized to fit `rect` (a status badge is ~14 px tall).
pub fn paint_particle_spinner(painter: &Painter, rect: Rect, time: f64) {
    let center = rect.center();
    let rx = rect.width() * 0.42;
    let ry = rect.height() * 0.30;
    let phase = (time * 2.1) as f32;
    let a = ring_point(center, rx, ry, 0.42, phase);
    let b = ring_point(center, rx, ry, 0.42, phase + std::f32::consts::PI);

    let breath = 0.45 + 0.55 * (0.5 + 0.5 * (time * 2.1).sin() as f32);
    painter.line_segment(
        [a, b],
        Stroke::new(1.0_f32, theme::accent(0.5).gamma_multiply(0.35 * breath)),
    );
    let dot = rect.height().min(rect.width()) * 0.17;
    paint_particle(painter, a, dot, theme::CYAN, 1.0);
    paint_particle(painter, b, dot, theme::VIOLET, 1.0);
}

#[allow(clippy::too_many_arguments)]
fn paint_ring(
    painter: &Painter,
    center: Pos2,
    rx: f32,
    ry: f32,
    tilt: f32,
    from: Color32,
    to: Color32,
    alpha: f32,
    width: f32,
) {
    let mut previous = ring_point(center, rx, ry, tilt, 0.0);
    for step in 1..=RING_STEPS {
        let t = step as f32 / RING_STEPS as f32;
        let point = ring_point(center, rx, ry, tilt, t * std::f32::consts::TAU);
        // Triangle-wave along the path so the gradient closes seamlessly.
        let mixed = theme::mix(from, to, 1.0 - (1.0 - 2.0 * t).abs());
        painter.line_segment(
            [previous, point],
            Stroke::new(width, mixed.gamma_multiply(alpha)),
        );
        previous = point;
    }
}

fn ring_point(center: Pos2, rx: f32, ry: f32, tilt: f32, angle: f32) -> Pos2 {
    let (sin_a, cos_a) = angle.sin_cos();
    let (sin_t, cos_t) = tilt.sin_cos();
    let x = rx * cos_a;
    let y = ry * sin_a;
    center + Vec2::new(x * cos_t - y * sin_t, x * sin_t + y * cos_t)
}

fn paint_particle(painter: &Painter, at: Pos2, radius: f32, color: Color32, alpha: f32) {
    painter.circle_filled(at, radius * 2.6, color.gamma_multiply(0.10 * alpha));
    painter.circle_filled(at, radius * 1.6, color.gamma_multiply(0.22 * alpha));
    painter.circle_filled(at, radius, color.gamma_multiply(alpha));
    painter.circle_filled(at, radius * 0.42, fade_white(alpha));
}

/// The correlation line: brightest when the pair is far apart, so it reads as a
/// connection being stretched rather than a static stick.
fn paint_link(painter: &Painter, a: Pos2, b: Pos2, alpha: f32, width: f32) {
    let strength = ((a - b).length() / 40.0).clamp(0.25, 1.0);
    painter.line_segment(
        [a, b],
        Stroke::new(
            width * 1.9,
            theme::accent(0.5).gamma_multiply(0.10 * strength * alpha),
        ),
    );
    painter.line_segment(
        [a, b],
        Stroke::new(
            width,
            theme::accent(0.5).gamma_multiply(0.42 * strength * alpha),
        ),
    );
}

fn fade_white(alpha: f32) -> Color32 {
    Color32::WHITE.gamma_multiply(alpha.clamp(0.0, 1.0))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn ring_points_stay_inside_the_bounding_ellipse() {
        let center = Pos2::new(50.0, 50.0);
        let (rx, ry) = (20.0, 8.0);
        for step in 0..RING_STEPS {
            let angle = step as f32 / RING_STEPS as f32 * std::f32::consts::TAU;
            let p = ring_point(center, rx, ry, RING_TILT, angle);
            let d = (p - center).length();
            assert!(d <= rx + 0.001, "point {p:?} escaped the ring");
            assert!(d >= ry - 0.001, "point {p:?} collapsed into the centre");
        }
    }

    #[test]
    fn the_pair_is_antipodal_through_the_centre() {
        let center = Pos2::new(0.0, 0.0);
        let a = ring_point(center, 10.0, 4.0, RING_TILT, 1.0);
        let b = ring_point(center, 10.0, 4.0, RING_TILT, 1.0 + std::f32::consts::PI);
        assert!((a.x + b.x).abs() < 1e-4, "{a:?} vs {b:?}");
        assert!((a.y + b.y).abs() < 1e-4, "{a:?} vs {b:?}");
    }

    #[test]
    fn tilt_actually_rotates_the_ring() {
        let center = Pos2::ZERO;
        let flat = ring_point(center, 10.0, 3.0, 0.0, 0.0);
        let tilted = ring_point(center, 10.0, 3.0, RING_TILT, 0.0);
        assert!((flat.y).abs() < 1e-6);
        assert!(tilted.y > 1.0, "tilted ring should lift off the axis");
    }
}
