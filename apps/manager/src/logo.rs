//! Resolution-free Entangled identity: two interlocked data loops, a luminous
//! correlation bridge and a small aperture at their shared centre. The same
//! geometry is rasterised once for the native window/taskbar icon.
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

    // Faint aperture where the loops cross. The diamond makes the mark read as
    // a designed product glyph rather than another generic atom logo.
    for (r, fade) in [(radius * 0.30, 0.05), (radius * 0.16, 0.10)] {
        painter.circle_filled(center, r, fade_white(fade * alpha));
    }
    let d = radius * 0.105;
    painter.add(egui::Shape::convex_polygon(
        vec![
            center - Vec2::new(0.0, d),
            center + Vec2::new(d, 0.0),
            center + Vec2::new(0.0, d),
            center - Vec2::new(d, 0.0),
        ],
        theme::accent(0.5).gamma_multiply(0.85 * alpha),
        Stroke::new(radius * 0.025, fade_white(0.8 * alpha)),
    ));
}

/// Native 64×64 taskbar/window icon generated from the same two-loop mark.
/// This is done once at startup and keeps packaging free of platform-specific
/// PNG/ICO drift.
pub fn app_icon() -> egui::IconData {
    const SIZE: u32 = 64;
    let mut rgba = Vec::with_capacity((SIZE * SIZE * 4) as usize);
    for y in 0..SIZE {
        for x in 0..SIZE {
            let px = x as f32 + 0.5;
            let py = y as f32 + 0.5;
            let nx = (px - 32.0) / 32.0;
            let ny = (py - 32.0) / 32.0;
            let edge = nx.abs().max(ny.abs());
            let corner =
                ((nx.abs() - 0.72).max(0.0).powi(2) + (ny.abs() - 0.72).max(0.0).powi(2)).sqrt();
            let inside = edge <= 0.78 || corner <= 0.20;
            if !inside {
                rgba.extend_from_slice(&[0, 0, 0, 0]);
                continue;
            }

            let mut r = 7.0;
            let mut g = 11.0;
            let mut b = 24.0;
            for (tilt, from, to) in [
                (RING_TILT, theme::CYAN, theme::VIOLET_DEEP),
                (-RING_TILT, theme::VIOLET, theme::CYAN_DEEP),
            ] {
                let (sin_t, cos_t) = tilt.sin_cos();
                let xr = nx * cos_t + ny * sin_t;
                let yr = -nx * sin_t + ny * cos_t;
                let ring = ((xr / 0.67).powi(2) + (yr / 0.29).powi(2)).sqrt();
                let coverage = (1.0 - (ring - 1.0).abs() / 0.065).clamp(0.0, 1.0);
                let color = theme::mix(from, to, ((nx + 1.0) * 0.5).clamp(0.0, 1.0));
                r += color.r() as f32 * coverage * 0.82;
                g += color.g() as f32 * coverage * 0.82;
                b += color.b() as f32 * coverage * 0.82;
            }
            let aperture = ((nx.abs() + ny.abs()) / 0.17).clamp(0.0, 1.0);
            let core = 1.0 - aperture;
            r += 120.0 * core;
            g += 210.0 * core;
            b += 255.0 * core;
            rgba.extend_from_slice(&[
                r.min(255.0) as u8,
                g.min(255.0) as u8,
                b.min(255.0) as u8,
                255,
            ]);
        }
    }
    egui::IconData {
        rgba,
        width: SIZE,
        height: SIZE,
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

    #[test]
    fn native_icon_has_the_expected_rgba_shape() {
        let icon = app_icon();
        assert_eq!((icon.width, icon.height), (64, 64));
        assert_eq!(icon.rgba.len(), 64 * 64 * 4);
        assert_eq!(icon.rgba[3], 0, "the outer corner stays transparent");
        assert_eq!(icon.rgba[(32 * 64 + 32) * 4 + 3], 255);
    }
}
